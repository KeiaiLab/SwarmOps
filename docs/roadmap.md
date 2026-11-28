# Roadmap

What SwarmOps builds next, and how each step will be checked. This page is the detailed roadmap;
the [README's roadmap](../README.md#roadmap) and the site's roadmap page summarise it, and
[docs/goals.md](goals.md) holds each goal's metric and target. Everything this page says exists is
on `stable` and cites where; everything else is **planned**. A target is a number SwarmOps sets for
itself: none is measured yet unless [docs/perf.md](perf.md) says so. Command names, flags, file
names and error codes of planned work are provisional until their milestone's plan fixes them.

## Goal

SwarmOps is a version control system for swarms of coding agents that works together with GitHub.
"Works together" means three setups, each natural for the people and tools in it:

| # | Local side | Remote side | Who lives there |
| --- | --- | --- | --- |
| 1 | git | SwarmOps | People and tools that keep stock git (`git clone`, `fetch`, `pull`, `push`) against a host where agents land into a store. |
| 2 | SwarmOps | GitHub | A swarm whose repository, CI, pull requests and deployments stay on GitHub; the store clones, fetches and pushes it directly, with no hand-run `git push`. |
| 3 | SwarmOps | SwarmOps | Stores on several hosts that sync natively and keep working offline. |

A topology counts as done when its acceptance criteria below pass in CI, with the three git
versions CI already builds where git takes part.

## Where we are

v0.1.0 (2026-10-20) read and wrote only this host's store and git repositories. v0.2.0 (M13,
[CHANGELOG](../CHANGELOG.md)) serves topology 2: a store clones, fetches, pushes and syncs a git
remote itself, through the user's own git, which carries everything that crosses hosts
([README](../README.md#swarmops-and-git), the "Offline, on one host" and "Sync between hosts"
rows). Topology 3 is not served yet. v0.3.0 (M5) adds workspaces: an agent builds and
tests its change on disk and lands the tree it tested, through `so ws` or MCP, with Claude Code's
worktree hooks ([Build and test a change](operate/build-and-test.md)). v0.4.0 (M6) adds `repair`,
`gc`, store `FORMAT 2`, with free-form branches, tags and remote-tracking refs, and `upgrade` from
FORMAT 1 ([Verify and repair](operate/verify-and-repair.md), [Making room](operate/making-room.md)).
v0.5.0, the first release of [To usable](#to-usable), ran the pilot on a real repository
([pilot.md](pilot.md#the-traders-pilot-a-real-repository)), measured G1, G2, G4, G9 and G17 and the
landing model ([perf.md](perf.md)), and holds the first v0.6 fixes: `ws commit` sends missing blobs
inline, `op log` lists committed operations only, and MCP `ws_create` fills its default path.
v0.6.0 fixes what the measurements missed: `land` settles a moved `main` inside the lock by metalog
merge, export memos a per-store key authenticates shorten `push` and `export`, a fetch checks every
object it receives, `main` cannot be deleted, and `just bench` gates the weekly run
([perf.md](perf.md#v060--2026-10-28)). v0.7.0 (M14) serves topology 1:
`swarmops serve` lets stock git clone, fetch, pull and push a store over ssh and smart HTTP
([Topology 1 today](#topology-1-today-local-git-remote-swarmops)).

### Topology 1 today: local git, remote SwarmOps

Served since M14 (v0.7.0). `swarmops serve` runs per connection, as an ssh forced command or a
web server's CGI program, and runs the host git's `upload-pack`, `receive-pack` and
`http-backend` on a serving repository inside the store, `git/serve.git`, whose refs are the
exports of the store's committed snapshot. Stock git clones, fetches, pulls and pushes with
protocol v2 or v0; a push moves a store ref by compare-and-swap, and a push to
`refs/land/<target>` lands. The store's `access.toml` says who may read, push, force and land
([Serve over ssh](operate/serve-over-ssh.md), [Serve over HTTP](operate/serve-over-http.md)):

```sh
git clone ssh://so-host/acme.git                  # or https://so-host/acme.git
git push origin HEAD:refs/land/main               # lands; `git fetch` brings the landed commit
git push origin feature/x v1.4.0                  # a branch and a tag, in a FORMAT 2 store
```

Criteria 1 to 5 below pass in `just check` with git 2.43.0; the Scheduled run's git matrix
passed them with git 2.39.5, 2.43.0 and 2.56.0 on `c7baf861` (2026-10-29). G16's three targets
are measured in [perf.md](perf.md#v070--2026-10-29). What it does not do yet is in
[known limitations](known-limitations.md#serving-requests): a land does not move the client's
`origin/main` until it fetches, a refused push's objects can be fetched over protocol v2 by id,
and the first connection to a large store exports its whole history once.

### Topology 2 today: local SwarmOps, remote GitHub

Served since M13 (v0.2.0), as [GitHub as a remote](guides/github-remote.md) and the
[daily loop](operate/daily-loop.md) run it:

```sh
so clone git@github.com:acme/acme.git --into "$SWARMOPS_REPO" --branch stable
# agents commit and land into main
so push origin main:review                   # CI runs on the push; a pull request from review
so sync origin --no-push                     # after GitHub merged it, by any method
```

What holds: commits fetched from GitHub keep their ids through any number of round trips; a push
is a compare-and-swap against what the last fetch saw, never forced; after a merge commit, a squash
or a rebase on GitHub, `so sync` recognises the changes GitHub holds and drops them, so the squash
merge that `git import` met with `E_GIT_IMPORT_DIVERGED` is now a state sync resolves; commits made
in the store can carry an address GitHub links to an account (`authors.toml`). What is missing:
pull requests opened from SwarmOps, and three of the four performance targets
([Topology 2](#topology-2-local-swarmops-remote-github)). Signed commits are planned and not built
([M42](#m42-signed-commits)), so a branch that requires signed commits refuses the store's
commits. The round trip through a bare clone, with
`git import`, `git export` and a hand-run `git push`, still works
([Git round trip](guides/git-round-trip.md)).

### Topology 3 today: local SwarmOps, remote SwarmOps

Two ways work, from v0.12.0, both through the server entry of M14
([A store as a remote](guides/store-remote.md)). Over the git wire, a store that `swarmops serve`
serves is a git remote of other stores: `so clone`, `fetch`, `push` and `sync` over ssh or HTTPS,
several hosts syncing one served store, each client landing onto the served head. Over `so://`,
with no git in between and over ssh alone, `clone`, `fetch`, `sync` and the `push` of a branch or
a tag: the upstream lands each change of `main`, and a changeset has one id on both stores.
Open of M11: its acceptance criteria, which need M9's simulator (partitions, two hosts pushing at
once, a process killed during a push), and its targets, which are not measured. Without a served
store, a git remote still carries history between two stores, one writing host at a time
([Git round trip](guides/git-round-trip.md#limits-to-know-first)).

### What is missing, in one table

| Needed | Topology | State on `stable` | Planned in |
| --- | --- | --- | --- |
| Packs sent and received over a connection | 1, 2 | Built (M13, M14): the user's git moves them between the remote and the store's mirror, and the host git serves them from `serve.git`; SwarmOps writes no pack, and the export writes loose objects ([known limitations](known-limitations.md#git-import-and-export)) | M13 (done), M14 (done) |
| Git's fetch and push protocols, as a client | 2 | Built (M13), by running the user's git | M13 (done) |
| Git's fetch and push protocols, as a server | 1 | Built (M14), by running the host git | M14 (done) |
| A server entry over ssh | 1, 3 | Built for git clients (M14): `swarmops serve` as a forced command; M11 plans `ssh <host> swarmops serve --stdio` ([architecture](architecture.md#crates-and-layering)) | M14 (done), reused by M11 |
| Remotes, remote state and `sync` | 2, 3 | Built for git remotes (M13): `remotes.toml`, the mirror's refs, `so sync`; built for store remotes over the git wire and over `so://` (v0.12.0), M11's acceptance open | M13 (done), M11 (store remotes) |
| Ref names beyond `main` and `release/<name>`, and tags | 1, 3 | Built (M6): free-form branches, `tags/<path>` and `remote/<remote>/<path>` in a `FORMAT 2` store; the git side maps them (M14): `git import`, `git export` and `serve` take branches and lightweight tags | M6 (done), M14 (done) |
| Access control and identities for remote callers | 1, 3 | Built (M14): the store's `access.toml`, the caller named by the key line or the web server, `E_AUTH` and `E_POLICY_DENIED` | M14 (done), M11 |
| Checks: a description of what runs for a tree | — | `.swarmops/checks.toml` with stages, `check list`, `check run` with findings and JUnit and SARIF reports, `check log`, run lines and `check runs` (M30-1, M30-2, M30-6, M31-1 to M31-5, M31-11 to M31-13); a checked land runs one command, the `check.argv` of `.swarmops/workspaces.json` | M30, M31 |
| Checks: results on a tree | — | Not built | M7, M32 |
| Checks: triggers | — | Not built | M33 |
| Checks: runners | — | Not built | M34 |
| Checks: secrets | — | Not built | M35 |
| Checks: logs and artifacts | — | Not built | M36 |
| Checks: rules beyond who may land | — | Not built | M37 |
| Delivery: deployments | — | Not built | M38 |
| A forge other than GitHub behind the bridge | 2 | Not built: any git host works as a remote (M13), and the planned bridge names GitHub alone (M32) | M41 |
| Signed commits | — | Not built: an imported commit's `gpgsig` header is kept verbatim, and a commit the store makes carries none | M42 |
| A process that stays | 1, 3 | Not built: `serve` runs once per connection | M43 |
| A store on a cluster | — | Built around the program: the server kit's Helm chart, one pod with one volume and `serve` per connection ([known limitations](known-limitations.md#the-server-kit-on-kubernetes)); no service, and no runner as a job | M44 |
| A server that refuses `{"file": "<path>"}` content and never passes on a host path | 1, 3 | Built for `serve` (M14), but git's own side-band lines ([known limitations](known-limitations.md#serving-requests)); M11's server reuses its guard | M14 (done), M11 |

## To usable

SwarmOps is **usable** when all of these hold:

1. A real repository, not SwarmOps' own, ran 8 or more agents for 3 or more days, and then: no
   commit was lost, `verify --deep` is clean, `rewritten` is 0, the export passes
   `git fsck --strict`, and the repository's own gate passes on the export.
2. [Goals](goals.md) G1 (50 processes), G2 (commit latency without syncs), G4 (landing under load:
   at 50 processes at least 20 lands/s, p99 under 500 ms, retries under 5 %) and G17 (a push at
   most 1.5× as costly on 12,000 commits as on 1,000) are measured in [docs/perf.md](perf.md) and
   met.
3. A benchmark gate in CI fails a regression of more than 20 %.
4. Backup and restore, `repair` and the rollback to git were exercised in that pilot
   ([Backup and restore](operate/backup-and-restore.md), [Verify and repair](operate/verify-and-repair.md),
   the pilot's [Rollback](pilot.md#rollback)).
5. No open soft finding touches data safety or the contract.
6. Two releases in a row shipped no data-safety fix.
7. For teams without GitHub: M14 (topology 1) is delivered. Met by v0.7.0.

The releases that get there:

| Release | Theme | Exit criteria |
| --- | --- | --- |
| v0.5.0 | Prove and measure | The pilot ran on a real repository of 993 commits, with condition 1's checks and condition 4's exercises recorded in [docs/pilot.md](pilot.md); G1 at 50 processes, G2 without syncs, G4 and G17 measured in [docs/perf.md](perf.md), met or not; the tests that read this host's repositories run in the Scheduled workflow; this section |
| v0.6.0 | Fix what the measurements miss | G1, G2, G4 and G17 met in [docs/perf.md](perf.md); the benchmark gate (condition 3), pulled forward from M9, in CI. Candidates, chosen by the measurements: the landing retry model; incremental export, fetch, push and sync; batched journal syncs only if G2 justifies them, and no `--durability fast` without a measurement in [docs/perf.md](perf.md) |
| v0.7.0 | M14 | Topology 1's acceptance criteria 1 to 5 pass in CI ([Topology 1](#topology-1-local-git-remote-swarmops)) |
| v0.8.0 | Safety and contract, first part | M15: 54 of 55 codes pinned, op ids an undo cannot redirect, a fetch that imports before it moves a tracking ref, `main` forward only, three workspace refusals, a land key scoped to the store, the land grant checked on each attempt; `.deb` and `.rpm` packages |
| v0.9.0 | Safety and load | M16: condition 5's remaining core rows closed in scope; G4 re-measured with one lock pass per land: p99 under 500 ms at 50 processes in one of two runs, so condition 2 is not met robustly ([perf.md](perf.md#m16--2026-11-07)) |
| v0.10.0 | Serve under load | M17: served reads off the writer lock, preparation, one pack per identical read; the prepared first clone 0.72× git's own server; G18's commit wait, G19's CPU, G20's land p99 at 50 processes and G22's memory missed, G21 not measured yet ([perf.md](perf.md#m17-baselines--2026-11-07)) |
| v0.11.0 | Take back, workspaces | M18's and M19's slices: `revert`, `ref log` (55 of 55 codes pinned), `land-check`, `land-group`, `ref txn`, `merge-base`; stale workspaces refused everywhere, edits snapshotted before an update; G4 at 50 processes on v0.10.0's batching met in two gated runs (p99 157.3 ms and 154.5 ms), its 32-process runs not recorded; the first sync after a clone missed (13.081 s against 1 s); G9's rows not measured again ([perf.md](perf.md#d2-8-gated-runs--2026-11-09), [perf.md](perf.md#m19-fan-in--2026-11-09-not-measured)) |
| v0.12.0 | `FORMAT 3`, update, macOS | M20's slices: FORMAT 3 the default of `init`, `upgrade` 2 → 3, verify and repair on it, `E_STORE_TOO_NEW` (56 of 56 codes pinned); its exit criterion, G18 at 1,000,000 records, is not measured yet ([perf.md](perf.md#m20--format-3-at-1000000-records--2026-11-12-not-measured)). M21's first slices, its exit criterion open. Beside them: `so update` and automatic update, logins, git's verbs on the command line, `so://` remotes, and the first macOS archive (M12). Still to do from this row's plan: M20 and M21 to their exit criteria below; the open soft findings triaged, each one that touches data safety or the contract fixed or recorded in [known limitations](known-limitations.md) with its target (condition 5) |
| v0.13.0 | The landing queue | M22's core, on a local store: `queue enqueue`, `status`, `withdraw`, `wait` and `run` and the four `queue_*` MCP tools; entries survive kill and power loss, candidates are checked in parallel, a red one is found by bisection and removed with a reason, the target moves only to a tree that passed; A22 met: 50 processes with a 30 s check each landed 50 of 50 once, 0 % retries, the longest hold of the lock 72.4 ms ([perf.md](perf.md#m22--the-landing-queue-under-50-processes--2026-11-18)). Beside it: `check list` and the check file. The queue's remote publish (M22-R) and M22-C1 to M22-C7 are still to do |

SwarmOps is declared usable at the first release where all seven conditions hold; condition 6
counts from the last release that shipped a data-safety fix.

**Status (2026-11-23).** v0.13.0 is released (M22's core, the landing queue), after v0.12.0 (M20's slices and M21's first ones, their exit criteria open; `so update`; the first macOS archive), v0.11.0 (M18's and M19's slices, their exit criteria open), v0.10.0 (M17), v0.9.0 (M16) and v0.8.0 (M15); v0.7.0 has M14, which serves topology 1 (condition 7), its criteria 1
to 5 pass in `just check` and in the Scheduled run's git matrix, and G16 is met
([docs/perf.md](perf.md#v070--2026-10-29): a clone 0.97× and a no-op fetch 1.82× git's own
server on 12,000 commits). G1 and G2 are met. G4 is met but for its p99 at 50 processes: 25
lands/s, 0 % retries and nothing lost, with a p99 of 2.5–2.7 s on the ext4 bench disk in M14's
window, for v0.6.0's code as for M14's (579.5–594.6 ms in v0.6.0's). M16 lands a change with its
commit in one lock pass (`land --changes`), and G4's harness lands so: at 50 processes its p99 was
441.7 ms in one run and 1,483.5 ms in another that started at twice the load, with 25.0 lands/s,
0 % retries and nothing lost in both ([perf.md](perf.md#m16--2026-11-07)). Condition 2 is not met
robustly, so M18 starts with group commit (D2): lands and commits that wait on the writer lock are
committed in batches (D2-1 to D2-7, in v0.10.0). On batching, in two runs started under the
measurement gate in a night window, G4 at 50 processes is met: p99 157.3 ms and 154.5 ms, 25.0
lands/s, 0 % retries, nothing lost
([perf.md](perf.md#d2-8-gated-runs--2026-11-09),
"D2-8 gated runs"); its 32-process runs, G1 at 50 × 1,000 with batched commits and the group-commit
design's slow sweeps are not recorded yet, so condition 2 stays open. G17's push and fetch are met
(1.02× and 1.34×, the fetch with the mirror's commit-graph); the sync of one land right after a
clone, M18's exit criterion, is not: 13.081 s on 12,000 commits and 18.8× the same sync on 1,000,
against 1 s and 1.5× (49.6 s in v0.7.0;
[perf.md](perf.md#m18-first-sync-after-a-clone--2026-11-08)). M19's workspace changes are in
v0.11.0; G9's update and commit rows wait for a quiet run, and the commit row stays missed until a
change feed exists. The benchmark gate (condition 3) runs in the weekly Scheduled run.
[Checks and delivery](#checks-and-delivery) (M30 to M40) is planned; of it, the check file and
`check list` (M30-1 and M30-2, step 1 of the first steps), a check's stages, `check run`, the
environment a check gets and its log with `check log` (M30-6 and M31-1 to M31-3, step 2) are in
0.13.0; a check's findings with its JUnit and SARIF reports (M31-4 and M31-5, step 3), and the run
line each check of a run leaves, with `check runs` and `check log --failed` (M31-11 to M31-13,
step 4) are on `stable` and in no release yet; nothing else is built. Step 2's CI half, the `check`
job starting the gate with `check run` and a pinned build, waits for a build of 0.13.0, the first
release that holds `check run`. It
is deferred past usable but for the units of M30 and M31 behind steps 1 to 8 of its
[first steps](#first-steps), which start now: they write nothing to a store, touch no data-safety
path and freeze nothing in the hash contract.

**Condition 5 (M15, in v0.8.0, and M16).** Not met yet. M15 closes the in-scope acceptance of the twelve
data-safety and contract rows the 2026-10-30 completeness report put on condition 5: 54 of the 55
error codes pinned by a golden envelope, operation ids an undo cannot redirect, a fetch that
imports before it moves a tracking ref, `main` moved forward only, three workspace refusals, a
land key scoped to the store and the land grant checked again on each attempt. What each row
leaves is recorded in [known limitations](known-limitations.md). M16 closes the rest of the core
rows in scope: on Unix, workspace writes that follow no link below the root; kill tests beside
readers, ref moves and keyed lands; `sync` and `git import` moving `main` only forward; a land
with its changes in one lock pass; `ancestry` along every parent; the land key judged under the
lock; and a `serve` push refused inside git's quarantine. Scored again after M16, 8 rows moved up
and none down. Still open: a key landed far below the land's base on another ref (M9), `release`
refs, tags and remote-tracking refs that move back, and crash tests for check results and
approvals, neither of which exists yet; revert's crash, kill and power-loss tests came with M18
([known limitations](known-limitations.md)).

Deferred past usable, with what stands in until then:

| Milestone | Why it waits | Stand-in |
| --- | --- | --- |
| M7, blame and review | No condition needs it, and its annotations need `FORMAT 3` (M20) | Review in GitHub pull requests (topology 2); `so log` and `so diff`; `git blame` on the export |
| M9's simulator | No condition needs it; only the benchmark gate is pulled forward, into v0.6.0; it needs M20 | M6's literal `SIGKILL` test, power-loss images, crash sweeps and fuzz targets |
| M11, native sync (topology 3) | No condition needs it, and it needs M14, M9 and M24 first | A served store as a remote, over the git wire or over `so://`, without M11's acceptance ([Topology 3 today](#topology-3-today-local-swarmops-remote-swarmops)) |
| M12, macOS and Windows | The pilot and CI run on Linux | The Linux releases; M12 is developed separately, and its macOS part builds, passes its tests and is packed by the release workflow, from v0.12.0 on |
| M20, `FORMAT 3` | One layout decision serves M7's records, the queue, withdrawal, lineage and the manifest that takes compaction out of the write; its design comes first | `FORMAT 2`; compaction inside the write |
| M21, health, drift and housekeeping | It reads the records M17 adds | `info`'s exit status; the envelope counts of [Monitoring](operate/monitoring.md#summarise-the-envelopes) |
| M22, the landing queue | Its core is pulled forward: it needs only `FORMAT 3`'s queue records and the workspace check; M7's required-check rules plug into it later | `ws land --check` in each process; the lock's own queue |
| M22-R, the queue's remote publish | The core proves the local queue first | land-direct; `so push` after a local queue land |
| M23, withdrawal and erasure | Its records need `FORMAT 3` | None: unreachable objects are counted, not removed |
| M24, rewriting with lineage | Its lineage id needs `FORMAT 3` | `rebase` and `op undo` |
| M25, moving onto a store without a pause | It needs M17's serve path | Follow a git remote, then switch (M13) |
| M26, an identity per agent from a short-lived credential | After 1.0, only when a pilot asks | ssh keys and web logins, one per identity |
| M30 to M40, checks and delivery | No condition needs it, and M32 onward need M7's check record. One exception: the units of M30 and M31 behind steps 1 to 8 of the [first steps](#first-steps) start now, because they write nothing to a store, touch no data-safety path and freeze nothing in the hash contract | Checks run by GitHub on the pushed branch (topology 2); `ws land --check` in each process |
| M41 to M44, a forge, signed commits, a service and a cluster | No condition needs them; M41's bridge needs M32 and M22-R, and M43's listener needs M17's serve path | A git remote, and `so push` after a local queue land; whoever pushes the export signs it; the server kit's chart, one pod with `serve` per connection |

## What the three share

Four pieces serve more than one topology. Each is built once, in the first milestone that needs it.

**The store's git mirror (M13).** A bare git repository kept inside the store's directory, owned by
SwarmOps and never written by people. Fetched packs land in it; `git import`'s existing code reads
it; `git export`'s existing code writes it; what leaves for a git remote is read from it. The
import and export keep their contracts: a commit imported from git exports back as itself, byte
for byte, and the same store gives the same commits every time
([architecture, Git export](architecture.md#git-export)). The mirror holds nothing the store and
the remotes cannot give again, but one hint: deleted, it is rebuilt by the next fetch and export.
Its refs record each remote's last known state (`refs/remotes/<remote>/<branch>`) and, since M16,
the remote commit `main` was last matched to (`refs/published/<remote>/<branch>`), the floor a
sync keeps the remote's head above; a rebuilt mirror lowers that floor to the sync's base. So
topology 2 needs no new ref name or journal record, and waits for no new store `FORMAT`. Built in M13 (`git/mirror.git`): it
takes no `FORMAT` step, but a marker of its own, `git/MIRROR`.

**Remotes (M13, built).** `so remote add <name> <url>` records a remote in the store's
`remotes.toml`; store-facing types keep carrying no path and no URL,
since `fetch`, `push` and `sync` are client-side operations, like `git import` today
([architecture](architecture.md#crates-and-layering)). The URL's scheme picks the protocol:

| URL | Protocol | Topology |
| --- | --- | --- |
| `https://github.com/org/repo` | git, smart HTTP, protocol v2 for fetch | 2 |
| `git@github.com:org/repo.git`, `ssh://host/path` | git over ssh | 2, or 1 seen from git |
| `so://host/path` | SwarmOps' own, over ssh | 3 |

**One sync engine (M13, built; extended by M11).** Unpublished lands are the store's changesets on `main`
above the remote's last known head. `so sync <remote>` fetches, lands each unpublished change onto
the remote's head with M4's three-way merge, in order, and pushes the result by compare-and-swap
against the head it fetched; a remote that moved meanwhile makes it fetch and land again, up to the
attempt limit `land` already has (`--max-attempts`, 8 by default). A conflict stops it with
`E_CONFLICT` and the report `land` gives today. Afterwards the store's `main` names the remote's
head, a move the op log records. `main` moves only forward there too (M16): a remote head that
does not hold, on its first-parent history, the commit the last sync or push matched `main` to is
refused with `E_REF_PROTECTED`, and a `git import --into main` of an ancestor tip moves nothing.
Against GitHub the client does the landing; against a store the remote does (M11).

**One server entry (M14).** `swarmops serve` runs per connection, started by ssh or by a web server
as a CGI program, so it adds no daemon. Under ssh, a key's line in the server's `authorized_keys`
runs it as a forced command, and the command the client asked for (OpenSSH passes it in
`SSH_ORIGINAL_COMMAND`) picks the protocol: `git-upload-pack` and `git-receive-pack` for git
clients, `swarmops serve --stdio` for stores. The same store answers both, so a host can serve
people with git and hosts with SwarmOps at once.

## Topology 1: local git, remote SwarmOps

### What people type

On the server, once, an operator makes a store and gives each person or tool a key line:

```sh
so init /srv/so/acme --name acme
so git import ~/src/acme --ref stable --into main
# ~/.ssh/authorized_keys of the account that serves:
#   command="swarmops serve --root /srv/so --as person/ada",restrict ssh-ed25519 AAAA… ada
```

On a laptop or a CI runner, nothing but stock git:

```sh
git clone ssh://so-host/acme                  # or https://so-host/acme over smart HTTP
git pull                                      # fetch and merge or rebase, as git does
git push origin main                          # a fast-forward, by compare-and-swap
git push origin HEAD:refs/land/main           # land: merged onto main, as an agent lands
git push origin feature/x                     # a branch of its own, once FORMAT 2 allows the name
git push origin v1.4.0                        # a tag, once FORMAT 2 allows tags
```

### How it works

**Protocol.** `git-upload-pack` answers git's protocol v2 (`ls-refs` and `fetch`, as
gitprotocol-v2 documents them) and the older version for clients that ask for it;
`git-receive-pack` answers pushes, which git sends over the older version, with the
`report-status` and `side-band-64k` capabilities. Packs follow git's pack format (gitformat-pack).
Smart HTTP follows gitprotocol-http, served as a CGI program behind a web server that terminates
TLS and authenticates.

**Ref mapping.** The store's `main` is git's `refs/heads/main`, `release/<name>` is
`refs/heads/release/<name>`, and `HEAD` points at `refs/heads/main`. Each advertised value is the
export of the ref's head: before it advertises, the server exports every ref it serves into its
serving repository, `git/serve.git`, from a committed snapshot, as `git export` does, so what a
client fetches is what the store holds and never what a crash could take back. In a FORMAT 2
store (M6) every branch `<path>` is `refs/heads/<path>` and every tag `tags/<path>` is
`refs/tags/<path>`, lightweight only; a FORMAT 1 store refuses them with a per-ref `ng`
(`E_FORMAT_REQUIRED`), and `remote/…` and other `refs/…` are not served.

**A push.** git's `receive-pack` writes the received pack into a quarantine of `serve.git`, checks
that every object the new tips reach is present and hashes to its id, then hands each command to
SwarmOps' `proc-receive` hook, which, for each ref the client named:

1. It imports the commits the new tip reaches that the store does not hold, as `git import` does:
   each becomes the changeset it already is in every store, byte-identical, refused by lossy class
   as today.
2. It moves the store ref from the changeset the client's old value names to the new tip's
   changeset by compare-and-swap. A client whose old value is not the ref's current export gets
   a per-ref `ng` with a stale reason, and git prints `! [remote rejected]`; nothing moves.
3. A move that is not a fast-forward is refused on `main` (`git push --force` included) and allowed
   elsewhere only as the access file says.

A push of several refs moves each by its own compare-and-swap: the server does not advertise
`atomic` until the store has multi-ref transactions, so `git push --atomic` fails on the client
before anything is sent.

**A push that lands.** A push to `refs/land/<target>` lands rather than moves: the server imports
the pushed commits, then runs M4's `land` of the tip onto `<target>`, three-way, squashed into one
changeset, retrying when the target moves, with a land key made of the pusher's identity, the tip's
id and the target, so that the same push run again replays rather than lands twice. Nothing is left
under `refs/land/`. The client sees its push accepted, and `git pull` brings the landed commit. A
conflict rejects that ref, and the report reaches the client as `remote:` lines on the side band
(the wording is the milestone's to fix):

```text
remote: E_CONFLICT: 1 path conflicts with main at 0eb07c3
remote:   src/greet.rs: both changed lines 12-14; main's change by agent/worker-2
remote: resolve it on top of main, then push again
To ssh://so-host/acme
 ! [remote rejected] HEAD -> refs/land/main (conflict)
error: failed to push some refs to 'ssh://so-host/acme'
```

**Fetch and clone.** `ls-refs` lists the exports; `fetch` sends a pack built from the mirror for
the commits the client lacks, negotiated as git negotiates. Shallow fetches (`--depth`), which CI
checkouts often use, are served; partial clone filters are not, in M14.

**Incremental and byte-identical.** An export writes only what the mirror's branch does not reach,
and an import takes only commits the store's external ids do not name (both exist today,
[architecture](architecture.md#git-import)). A commit that came from git is served back as itself,
so a clone of the store shows the same commit ids as the repository it was imported from.

**Identity and access.** The forced command or the web server names the caller's identity
(`person/ada`, `ci/runner-3`), which the journal records for every ref move and land, as `--as`
does today; commits keep their git authors and committers, byte for byte. An access file in the
store says which identity may read, push to which refs and land onto which targets; a refusal is
`E_AUTH` or `E_POLICY_DENIED`, the codes the schema already holds, sent as the `ng` reason. The
server refuses `{"file": "<path>"}` content and passes on no host path
([known limitations](known-limitations.md#serving-requests)).

**Offline.** The git client is offline as git is: commits, branches and history are local, and
`fetch` and `push` need the server. Agents on the server's host keep committing and landing while
no client is connected.

### What it builds on

M3's import and M8's export (the mirror's content), M4's `land` (push to `refs/land/`), M13's pack
code and mirror, M6's FORMAT 2 (branch names, tags) and committed snapshots (M8). Workspaces (M5)
are independent of it.

### Deliverables

- `swarmops serve`: the ssh forced-command entry and the CGI entry, the upload-pack and
  receive-pack sides, the quarantine and connectivity check.
- The `refs/land/<target>` push, its land key and its conflict report on the side band.
- The access file, and `E_AUTH` and `E_POLICY_DENIED` raised for the first time.
- Shallow fetches; branches and tags once FORMAT 2 holds them.
- [Operating guide](operations.md) pages: serving a store over ssh and over HTTP.

### Acceptance criteria

Each runs in CI, offline: ssh through a `GIT_SSH_COMMAND` stand-in that starts the forced command
locally, HTTP through a loopback server that runs the CGI program.

1. With git 2.39.5, 2.43.0 and 2.56.0, each of `git clone`, `git fetch`, `git pull --rebase`,
   `git pull --no-rebase`, `git push` (a fast-forward, a new branch, a tag, a deletion where the
   access file allows it) and `git clone --depth 1` succeeds over ssh and over HTTP, with protocol
   v2 and with the older one.
2. Round trip: a repository imported into a store and cloned from `serve` has the same id at every
   ref (`git rev-parse`), and `git fsck --strict` passes on the clone.
3. A push whose old value is stale moves nothing and exits non-zero on the client; the store's
   `verify --deep` stays clean.
4. A push to `refs/land/main` lands exactly once under 8 concurrent pushers and agents landing at
   the same time; the same push run again replays. A conflict exits non-zero, writes nothing, and
   prints the conflicting paths.
5. A key without write access gets `E_AUTH` or `E_POLICY_DENIED` as its `ng` reason; a request
   naming `{"file": …}` content is refused.

### Targets

Measured by `just perf`'s `serve_perf` cases ([perf.md](perf.md#v070--2026-10-29)).

| Target | How it is measured |
| --- | --- |
| A no-op `git fetch` costs at most 2× the same fetch from git's own `git upload-pack` serving a bare copy of the same repository | Both on one host, through the same ssh stand-in, on the pilot's repository |
| A `git clone` costs at most 2× git's own, for the same repository | As above |
| A push to `refs/land/main` of one commit returns in under 1 s on a history of 12,000 commits | The push timed by the client, the store under no other load |

### Risks and open questions

- Building packs and resolving deltas costs memory: received packs need bounds like the import's
  (each file up to 512 MiB, today's cap).
- Advertising runs an export of every served ref, which today reads the whole history below its
  head ([known limitations](known-limitations.md#git-import-and-export), "An export reads the whole
  history below its head"): an authenticated cache, unscheduled today, may become M14's work.
- Annotated tags are git objects a changeset does not hold: a new object kind with its own header,
  never a change in place of the hash contract, or bytes kept in the mirror only. Open.
- Whether `git push origin main` should also land rather than only fast-forward: open, decided with
  the pilot's users; `refs/land/` stays either way.

### Non-goals in M14

Partial clone filters, git's bundle URI and server-side user hooks (SwarmOps has no user hooks),
signed pushes, git LFS, submodules (a pushed gitlink is refused, as the import's lossy class refuses
it today), and a web interface.

## Topology 2: local SwarmOps, remote GitHub

### What people and agents type

```sh
so clone https://github.com/acme/acme --into ~/.local/state/swarmops/repos/acme
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme
so remote list                                 # origin, and the branch main follows
# agents commit and land into main, through MCP or the CLI
so fetch origin                                # what GitHub gained; main does not move
so sync status origin                          # lands ahead of GitHub, commits behind it
so push origin main:review                     # a review branch, for a pull request
so sync origin                                 # fetch, land on top of GitHub's main, push
so push origin main                            # a fast-forward of GitHub's main, nothing more
```

`so clone` is `so init`, `so remote add origin <url>`, a fetch of every branch and an import of the
default branch into `main`. A pull request is opened from the pushed branch with GitHub's own tools;
SwarmOps calls no GitHub API in M13.

### How it works

**Protocol.** `so fetch` speaks git's protocol v2 over smart HTTP (gitprotocol-v2,
gitprotocol-http) or over ssh, through the system's `ssh` as git does; `so push` sends a pack and
its ref updates over the receive-pack exchange. Received packs are kept in the store's git mirror
with their index.

**Ref mapping.** A fetch moves the mirror's `refs/remotes/origin/<branch>` for every branch on
GitHub, and imports what they reach into the store as changesets, with no store ref moved: the
remote's state is the mirror's refs, read through the store's external ids. The store's `main`
follows one branch (the default branch, or `--branch`). A push names the store ref and the GitHub
branch (`main:review`); tags stay GitHub's and are fetched into the mirror only.

**A push is a compare-and-swap.** `so push` exports the store ref into the mirror, then asks
GitHub to move the branch from the value the last fetch saw to the export; a branch that moved
meanwhile is refused by GitHub as git's own push would be, and `so push` fails with a divergence
error (proposed: `E_REMOTE_DIVERGED`, exit 3) and the hint `so sync origin`. A rejection by a
branch rule or ruleset comes back with GitHub's message in `details` (proposed: `E_REMOTE_REJECTED`).
`so push` never forces a branch; a branch that must be rewritten is rewritten with git.

**Sync lands, the client side.** `so sync origin` is the engine in
[What the three share](#what-the-three-share): GitHub cannot land, so the store lands its unpublished
changes on GitHub's head and pushes by compare-and-swap. Changes GitHub already holds drop out: a
pull request merged with a merge commit continues the export's history; one merged by squash or by
rebase leaves the store's change with nothing to land on top of GitHub's head, and the sync drops
it rather than land it twice. A change is recognised in GitHub's history by its
`SwarmOps-Changeset:` trailer, which a rebase keeps and a squash keeps only when GitHub's squash
message keeps the commit bodies; otherwise by the change applying to the remote's head as a no-op.
The rule is M13's to fix and test. Today's `E_GIT_IMPORT_DIVERGED` after a squash merge becomes a state
`sync` resolves.

**Incremental and byte-identical.** A fetch requests only what the mirror lacks; the import takes
only commits the store's external ids do not name; the export writes only what the GitHub branch's
last export does not reach. Commits fetched from GitHub keep their ids through any number of round
trips; commits made in the store export to the same ids every time.

**Authentication.** Over HTTPS, the token GitHub documents for git over HTTPS (a personal access
token or a GitHub App installation token), taken from git's credential helpers when git is
installed, as `git credential fill` gives it, or from an environment variable in CI; over ssh, the
user's keys through `ssh`. The store writes no credential to disk. Since 0.12.0 a login
(`auth login`) keeps a token outside every store, in one file of the user's, for an https remote
that names no variable.

**Identity mapping.** A commit fetched from GitHub keeps its author and committer; its changeset
records no agent, as an import does today. A changeset made in the store exports with its agent as
author; GitHub links a commit to an account by its email, so a per-identity author address (an
agent family to a bot account's address, say), recorded when the changeset is made, lets branch
rules that require known authors accept it. The journal records the identity behind each move of
a store ref that a `sync` makes.

**Offline.** Every local command works without a network. `fetch`, `push` and `sync` fail with a
retryable error (proposed: `E_REMOTE_UNREACHABLE`, exit 4) and change nothing; lands made meanwhile
wait as unpublished, and `sync status` counts them.

### As built: where M13 departed from this plan

M13 is delivered (v0.2.0). This section's plan stays as it was written; M13 built it with these
departures, each a decision of its design:

- **The transport is the user's git**, run as a child process for fetch, push and `ls-remote`,
  not git's protocols written in SwarmOps: credential helpers, proxies and ssh settings apply as
  the user configured them, SwarmOps adds no network crate and writes no pack, and the tests run
  git's own `http-backend`. gix stays on the disk side (import, export, the mirror).
- **A fetch imports the followed branch only.** Every branch and tag is fetched into the mirror;
  only the branch `main` follows is imported, and another branch when a push names it. Tags are
  never imported.
- **Credentials** are git's (helpers, ssh keys), or a token in an environment variable the remote
  names (`--token-env`); no `GH_TOKEN` or `GITHUB_TOKEN` is picked up unasked. Since 0.12.0 a
  token the user logged in with (`auth login`) is a third, asked for by logging in; its device
  flow runs the user's curl, so SwarmOps still adds no network crate.
- **`sync` moves `main` before it pushes**, so a refused push leaves `main` landed on the remote's
  head (`details.main_moved`; landing a change that reverts it takes it back); `sync --no-push`
  serves branches that take pull requests only. A change GitHub holds is recognised by any of
  three trailer lines (`SwarmOps-Changeset`, `SwarmOps-Squashed-From`, `SwarmOps-Land-Key`),
  indented or bulleted as a squash message lists them, or by merging as no change.
- **The per-identity address** is the store's `authors.toml`, keyed by agent family; it changes
  what new changesets record, and not the export's rendering (version 1 of `git_oid`).
- **`E_REMOTE_REJECTED` exits 5** (a rule or a credential), and the MCP tools are `remote_list`,
  `fetch`, `push`, `sync` and `sync_status`; `clone` and `remote add|remove` are CLI only.
- **The targets**: fetch met its 1.5×; push (3.86×), sync (8.76 s) and clone (1.29×) missed theirs,
  recorded in [perf.md](perf.md#git-remotes-m13-the-three-topology-2-targets) and the
  [known limitations](known-limitations.md#git-remotes), and none blocked the milestone.
- **Criterion 4** passed in the live run against a private scratch repository, and the pilot
  ([pilot.md](pilot.md#the-m13-pilot-a-store-and-github)); the weekly live job runs it again.

### What it builds on

M3's import and M8's export, unchanged in their contracts; M4's `land` for sync; the external ids
that let import and export continue each other. It needs no new store `FORMAT`, since remote state
lives in the mirror.

### Deliverables

- Pack reading and writing; the client sides of git's fetch and push over HTTPS and ssh.
- The store's git mirror; `so clone`, `so remote add|remove|list`, `so fetch`, `so push`,
  `so sync`, `so sync status`; their MCP tools, so that an agent syncs without a shell.
- Credentials from git's helpers or the environment; the per-identity author address.
- The proposed error codes, pinned by golden envelopes.
- A guide that replaces the bare clone of the [daily loop](operate/daily-loop.md) with
  `so fetch`, `so push` and `so sync`.

### Acceptance criteria

Offline in CI: the remote is a bare repository served on loopback by git's own `git http-backend`
and over the ssh stand-in, made by git 2.39.5, 2.43.0 and 2.56.0. The GitHub run is opt-in
(`SWARMOPS_GITHUB_LIVE=1`, a scheduled job against a scratch repository), as tests on this host's
real repositories are today (`SWARMOPS_HOST_REPOS=1`).

1. Round trip: `so clone`, then `so push` of the untouched `main` to a new branch, leaves the
   remote's ids unchanged at every pushed ref; `rewritten` is 0 and `git fsck --strict` passes on
   the remote.
2. After agents land, `so push` moves the remote branch by a fast-forward; run again, it sends
   nothing; a remote that moved makes it fail and change nothing.
3. `so sync` after each of git's three merge shapes on the remote (a merge commit, a squash, a
   rebase of the store's branch) ends with the remote holding every landed change once and the
   store's `main` at the remote's head.
4. Live, on GitHub: a commit `so push` sends triggers the repository's `push` workflow with the
   workflow file unchanged; a pull request from the pushed branch merges by each of GitHub's merge
   methods, and the next `so sync` completes.
5. GitHub's documented limits are constraints the client checks before it sends: a file GitHub
   refuses for its size (GitHub documents 100 MiB, and a warning at 50 MiB) is refused locally with
   its path, and so is a push over GitHub's documented push size (2 GB); the numbers are taken from
   GitHub's documentation when M13 starts.
6. With the network gone, `fetch`, `push` and `sync` fail retryably and every local command runs.

### Targets

Each is a `just perf` case against a loopback remote, so the network does not count; M13's first
run, on a loaded host, is in [perf.md](perf.md#git-remotes-m13-the-three-topology-2-targets).

| Target | How it is measured |
| --- | --- |
| `so clone` costs at most 1.1× `git clone --bare` followed by today's `git import` of the same repository | Both timed on one host, the pilot's repository and a fixture of 12,000 commits |
| A `so fetch` or `so push` of one commit costs at most 1.5× as much on 12,000 commits of history as on 1,000 | The store-side time of each, from `meta.elapsed_ms` |
| A `so sync` of one land onto a remote that gained one commit returns in under 1 s on 12,000 commits | Timed by the client |

### Risks and open questions

- The mirror doubles the bytes a repository takes on disk; whether it can hold packs only, or the
  store can serve packs from its own objects, is measured in M13.
- The second target meets today's cost of reading the whole history below a native head
  ([known limitations](known-limitations.md#git-import-and-export)): the authenticated cache may be
  M13's.
- A per-identity author address changes what a new changeset records, not how an old one exports;
  whether it needs a new version of the export's rendering is decided in M13's plan, under the
  versioning policy.
- Opening pull requests from SwarmOps, through GitHub's documented REST API: open, after M13.

### Non-goals in M13

GitHub's issues, reviews and checks APIs; git LFS and submodules (refused or dropped by class, as
the import does today); SHA-256 repositories (refused, as today); shallow or partial clones of the
remote (the store holds full history).

## Topology 3: local SwarmOps, remote SwarmOps

### What people and agents type

```sh
so clone so://hub/acme --into ~/.local/state/swarmops/repos/acme
so remote add hub so://hub/acme              # or, in a store that exists
so fetch hub                                 # resumable; no negotiation
so sync status hub
so push hub                                  # each unpublished land, landed on hub's main
so sync hub                                  # fetch, push, and main moved to hub's head
```

### How it works

**Protocol.** `so://host/path` runs `swarmops serve --stdio` on the host over ssh, through the
server entry of M14: the same serde requests and responses as every operation today, one JSON
message a line. The requests M11 adds ask for what the client lacks: changesets after a head the
client knew, the missing subtrees of a root, the missing objects by id. The client negotiates
nothing: content addressing makes the store's own objects the list of what it has, and an
interrupted fetch resumes without asking for an object again.

**Ref mapping.** One remote is the upstream; its `main` and `release/<name>` are the shared refs.
Remote-tracking refs (`remote/<remote>/<ref>`) are store refs that only `fetch` moves, which needs
FORMAT 2 (M6).

**Push lands on the upstream, exactly once.** `push` uploads the objects the upstream lacks and
asks the upstream to land each unpublished change in order, with the change's own id as the land
key; the upstream runs M4's compare-and-swap loop, so its `main` stays linear, and a conflict
returns M4's report to the change's agent. A push cut short resumes from a record written before
anything is sent, and the land key makes a repeated land a replay. Changesets landed upstream get
new ids, and record the ids they came from.

**Identity and access.** The ssh key's forced command names the identity, and the upstream's access
file decides what it may land, as in topology 1. The lander upstream is that identity; the change
keeps its agent.

**Offline.** As M11 is designed: every operation runs on the host's own
store; sync needs the network and fails retryably without it.

### What it builds on

M3's external ids, M4's `land`, M6's FORMAT 2, M7's rules for carrying approvals across stores, M8's
committed snapshots, M9's simulator for partition tests, and M13's remotes and sync engine with
M14's server entry.

### Deliverables

M11 as the README states it (a full store on every host, `fetch`, `push` and `sync status` against
one upstream, over ssh), plus `so clone so://…`, `so remote`, and `so sync`, shared with topology 2.

### Acceptance criteria

1. Under M9's simulator: partitions, reconnections, two hosts pushing at once and a process killed
   during `push` leave the upstream's `main` linear and every confirmed local land on it exactly
   once.
2. `fetch` and `push` run again change nothing; a fetch cut short and resumed fetches no object
   twice.
3. A host with no network commits, lands, reads and verifies as before.

### Targets

Not measured. A fetch cut short and resumed transfers no object twice (counted by the client). A
`so sync` of one land over loopback ssh returns in under 1 s on 12,000 changesets (timed by the
client).

### Risks and open questions

- Local changeset ids change when they land upstream: tools that kept a local id must follow the
  recorded origin.
- Approvals carried between stores depend on M7's rules.

### Non-goals

Several upstreams merging with each other, partial replicas and a caching tier.

## Checks and delivery

**Planned: nothing in this section is built.** SwarmOps takes on a role beside its three
topologies; the role is not a fourth one. SwarmOps describes what must be checked for a tree,
decides what a given change needs, runs those checks on the caller's host or on runners that ask
for work, records each result against the tree it is for, admits a land only on recorded results,
and delivers from a store. Until then, checks run on GitHub for the pushed branch (topology 2),
and `ws land --check` runs one command in each process
([Build and test a change](operate/build-and-test.md)).

Five properties hold across the role, and each is something a reader can test:

- a result is keyed by tree, so identical trees are checked once;
- "not applicable" is an answer that satisfies a gate, so nothing waits for a result that will not
  come;
- admission is decided before any runner starts;
- a land is checked on the tree it will produce;
- every answer is one JSON envelope with a code, `retryable` and `hint`.

**What is settled.** The maintainers decided on 2026-11-18: the milestone ids M30 to M40 and the
goal numbers G25 to G30; the check file's place and version, `.swarmops/checks.toml` with
`version = 1`; four command families, `check`, `runner`, `policy` and `deploy` (`queue` is M22's);
that no daemon is added; what the open-source program holds and what a hosted service holds
([The hosted service](#the-hosted-service)); and that M30 and M31 start now, with steps 1 to 8 of
the [first steps](#first-steps). The rest of the design below is what each milestone's plan
confirms or replaces before its first unit is built. Three questions are open, and one point waits
for the maintainers ([Risks and open questions](#risks-and-open-questions-3)).

### What people and agents type

Planned and provisional: none of these commands exists, and each milestone's plan fixes its names.

```sh
so check plan my-change                        # which checks this change needs, and why
so check run --stage before-land               # in a workspace: run the plan's checks on this host
so check status my-change                      # the roll-up: what a land still waits for
so check wait my-change                        # one blocking call; the exit status is the verdict
so runner run --follow                         # on a runner's host: ask the store for work
```

Where no store is at hand, `check list`, `check lint`, `check plan` and `check run` take
`--dir <path>`: they read the check file from that directory, and the caller gives the changed
paths. Every answer of this directory form says that the caller vouched for the file and the
paths, and a gate never accepts one.

### How it works

**Definitions.** A tree says what its checks are in one versioned file, `.swarmops/checks.toml`,
read from the store at the tree being judged; `swarmops schema` gives its JSON form. A check is
one command: an argv with no shell, with declared inputs, a stage, the checks it needs first,
variants and the labels its runner must hold. Steps, conditions and composition inside a check
stay in the repository's own task runner, as with `ws land --check` today. That has a price:
reuse, running again and selection work per check, so a gate that is one recipe gets none of them
until its repository splits it. The `check.argv` key of `.swarmops/workspaces.json` keeps working,
with the environment it has today. SwarmOps does not run GitHub's workflow files: an importer
reads them once, as a public file format, writes a check file for the part that maps, and lists
the rest.

**Plans and inputs.** A check declares the paths it depends on; one that declares nothing takes
the whole tree. For a change, `check plan` answers which checks must run and which are not
applicable, with one reason each, from the store alone and before anything starts. A plan fails
closed: inputs it cannot resolve, or a definition the candidate changed, turn the check to `run`.
No second mechanism filters triggers by path. Selection may skip a required check only after
evidence: `not_applicable` satisfies a gate on a target once that target has run in shadow mode
for a number of lands the operator sets and counted no miss. Until then a plan informs, and reuse
of an identical key is the only saving.

**Results and reuse.** Results live in three layers. The verdict is M7's Check record, keyed by
the tree and by a check key. The key is built from a version, the check's name, its definition
digest, its input digest, its variant values, its image digest, its declared variables' values,
its labels and an environment stamp the runner's operator sets. It is a new derived kind of the
hash recipe, with golden vectors: nothing existing changes, a different recipe later is a new
kind, and the kind is frozen with M32-1, not earlier. The execution (wanted, claimed, started,
finished, cancelled) is a record kind of its own. Logs and artifacts are files beside the store,
addressed by digest and synced to disk before the record that names them. A passing result under
the same key is taken in place of a run; a failed or cancelled result is never reused, and a
result is reusable only when a granted recorder wrote it. Only journal records decide: the run
lines a check run leaves are diagnostics, as the serve records are. For an agent, `check status`
gives the roll-up and `check wait` blocks until every required check is final; findings come back
as data, bounded, in the envelope; and the outcomes that are not failures are each distinct:
`superseded`, `not_applicable`, `reused`, `expired`, `stalled`.

**Triggers.** Six things make a run wanted. A land is proposed: its wanted runs are written in
the same operation as the queue attempt, so a land never waits on a pass. The other five are read
from the journal or the clock by `check tick`, a pass over records: a ref moved, a tag was
created, a change request has a new revision, a schedule is due, someone asked by hand. No command
runs inside the write path. A replaced candidate is never run, and bounds per identity refuse a
request before anything is written. Every command that asks for something takes `--key`, as
`land --key` does, so a retry is a replay. The store host's clock decides leases, maximum ages,
schedules and waits.

**Runners.** A runner is a client: it claims a wanted run under a lease, runs it and records the
result, over the native wire that `serve --stdio` answers, from the store's host or from any host
that reaches `serve`. The store never calls out, and the process that answers a connection runs no
command a repository defines. No daemon is added. A runner and `check tick --follow` are
long-running clients an operator starts, as `derive --follow` is; schedules come from the host's
own timer; the store's records are the only queue; no SwarmOps process listens on a socket. The
role needs none: [M43](#m43-a-resident-service) plans an optional service that may start these
clients' passes, and a store without it runs them as said here. What a
stopped client looks like is part of the contract: the roll-up names the last pass's frame and age
and each waiting run's age, `check wait` ends `stalled`, and an alert rule fires. An ask that
finds nothing takes no writer lock. A check's use of the host is bounded: its time, with a maximum
beside the store that a candidate cannot raise; memory, processes and disk through the container
runtime, and for a host-run check as far as the platform allows without `unsafe`, with the answer
listing what was not applied. A runner refuses a work directory on the store's file system,
because a full disk fails the journal's sync and stops every land.

**Trust and secrets.** The access file sorts identities into trust classes. An untrusted run gets
the target's check definitions, not its own; only a runner whose grant gives `isolated` may claim
it; it gets no secret, no kept state and no network. A held class waits for approval. Secrets live
only on the runner's host, in a file outside every store: a check file declares names, a value is
released when check, target, environment and trust class all match, and values are masked before
anything leaves the runner. A secret never enters an object, a record or a log, and until M35-5
exists no secret is put on a SwarmOps runner at all. The promise is bounded: the candidate's own
scripts still run, the container is the host runtime's, and the `isolated` label is not offered on
macOS. The program alone does not defend a host against hostile code; a machine per job, behind
`runner run --once` and the operator's commands, is the way to run a stranger's code.

**Delivery.** A rules file beside the access file names environments: each a delivery target with
the refs allowed to reach it, its approvers, its wait and the scope of its secrets. A deployment
is a record, a check with an environment becomes claimable only once that environment's rules
pass, and its secrets are released then. A release is an immutable tag with notes and assets kept
by digest. A build record in the program is unsigned: it shows that a file is the one the store's
record names, not who built it.

**The bridge to GitHub.** Where a repository's checks still run on GitHub, a store can offer a
queue attempt there as a candidate branch (`check offer`), read GitHub's verdict into its own
records (`check fetch`), and write its own roll-up back as a commit status (`check publish`). So
the first gate that a store decides can be run by GitHub's runners: the store holds the queue, the
attempt and the verdict, and the landed commit is the one that was checked. Whether this
repository takes that path depends on two of the open questions below.

**Where the gate defends and where it guides.** Only a result recorded by an identity the access
file grants for that check name counts. The gate *defends* only when three things hold: the
recorder is granted; the recorder reaches the store through `serve` with its own key, not through
the files; and the checked code runs as another OS user or in a container, so that it can read
neither that key nor the store's directory. `runner check` says which of the three hold. Where one
is missing, an identity is whatever `--as` says, and the gate *guides*.

### What it builds on

- **M5** (done): workspaces, `.swarmops/workspaces.json` and `ws land --check`, which already runs
  one argv with no shell.
- **M7** (planned): the Check record and the landing gate that reads results. This role extends
  them and does not define them.
- **M13** (done): the git remote. The three bridge commands and `release publish` are its clients.
- **M14** (done): `serve`, its entrances and the access file, which gains the `check` and `run`
  grants.
- **M18**: `land-check`, which a plan reads.
- **M19**: the second round of workspaces. Its change feed is not built, so the two loops poll
  until it exists.
- **M20**: `FORMAT 3`, whose named features hold the new record kinds (`check`, `run`, `deploy`).
- **M21**: health windows, alert rules and the metrics output. M40 adds figures and rules to them;
  there is no second metrics path.
- **M22**: the landing queue, built on a local store; its attempt (M22-C1) is proposed.

Nothing is planned a second time: where the role needs work from M7, M22, M11 or M26, it proposes
a slice to that milestone's plan ([below](#proposed-slices-of-existing-milestones)) and waits for
the answer.

### Deliverables

One unit is what one slice ships: one command, one record kind, one file format, one protocol or
one guide. A unit's id is its slice id. It lands when what it depends on is in, and its record is
then the status file `docs/status/<milestone>/<slice-id>.txt`. The eleven tables below hold the
169 units of M30 to M40; the twelve proposed slices of M7, M22, M11 and M26 follow, and are not
counted among the 169. "Depends on" names milestones, proposed slices and units that must exist
first. Every unit's state is `planned`, `on stable` once it lands, and the release that first
holds it once one does (`in X.Y.Z`).

What holds for every unit:

- Its acceptance is a test in `just check`, offline. The three bridge units that talk to GitHub
  (M32-10, M32-11, M38-13) have an offline half against recorded answers and a live half that
  runs weekly with a token.
- No acceptance states a speed or a duration: order is tested by the order of events or records,
  and the targets are those of [Targets](#targets-3), not measured.
- Every reader of input a stranger may write (the check file, a check's report file, a JUnit
  file, a SARIF file, a schedule, the files the importer reads) gets a fuzz target, as G14 asks.
- A refusal by a rule is `E_POLICY_DENIED` with the rule in `details`. Five error codes are
  proposed, each fixed by its milestone's plan: `E_CHECK_FILE`, `E_CHECK_PENDING`, `E_RUN_HELD`,
  `E_RUN_LIMIT` and `E_RUNNER_NONE`.
- Every command of M30 and M31 also has the directory form.
- The tables use some longer provisional spellings (`release …`, `notify …`); they are placed
  under the four command families when built, as `deploy release` and `check notify`.

#### M30: Check definitions and plans

What a tree says should run, and what a given change would start. It reads; it writes nothing to a
store and freezes nothing in the hash contract. 17 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M30-1 | The check file `.swarmops/checks.toml`, `version = 1`: named checks, each one argv with no shell, a directory and a timeout; read from the store at a named changeset, or with `--dir` from a directory, in which case every answer says the caller vouched for the file | M5 | in 0.13.0 |
| M30-2 | `check list`: the checks of a changeset's file, or of a directory's, each with the digest of its definition | M30-1 | in 0.13.0 |
| M30-3 | Declared inputs: a check's path patterns, in the access file's `*` and `**` grammar, resolve against a tree to the entries they match; no pattern means the whole tree | M30-1 | planned |
| M30-4 | `check plan`: for a changeset against its target, a workspace's edits, a `land-check` item or, in the directory form, a list of changed paths, each check marked `run` or `not_applicable` with one reason from a closed set; no lock, nothing written | M30-3, M18 | planned |
| M30-5 | Planning fails closed: unresolvable inputs, an unreadable path, a renamed or deleted definition, or a definition that differs from the one at the target's head widen the check to `run`, with the reason `widened` or `definition_changed`; a check whose definition changed is never `not_applicable` or `reused` for that land; an error never yields an empty plan | M30-4 | planned |
| M30-6 | Stages: a check names when its answer is needed, `before-land`, `after-land`, `scheduled`, `release`, `manual` or `deploy`; `check plan --stage` | M30-1 | in 0.13.0 (`check plan --stage` with M30-4) |
| M30-7 | Order between checks: `needs` lists the checks that go first; a failed need skips its dependants unless they say `always`; a cycle is refused at parse | M30-1 | planned |
| M30-8 | Variants: a check expands over declared value lists into named variants, with exclusions, added combinations, a cap on the count and a stop-at-first-failure switch | M30-1 | planned |
| M30-9 | Runner requirements: a check names the labels its runner must hold and a size hint; `check plan` reports them | M30-1 | planned |
| M30-10 | The owner map: path patterns mapped to the checks a change there also runs, and checks that always run, for dependencies no input pattern expresses | M30-4 | planned |
| M30-11 | The selection switch: a flag, or a setting beside the store, forces every check to `run`, and each plan says so | M30-4 | planned |
| M30-12 | `check plan --explain <check>`: which changed paths matched which input or owner pattern | M30-10 | planned |
| M30-13 | `check import`: reads the GitHub workflow files of a tree and writes a check file for what it can express (commands, order, variants, timeouts, paths), listing every key it could not; schedules are listed as untranslated until M33-7 exists | M30-3, M30-7, M30-8 | planned |
| M30-14 | Guide "Describe a repository's checks", with this repository's file as the example | M30-4 | planned |
| M30-15 | Includes: a check file takes definitions from other files of the same tree by path; nothing crosses an include but definitions | M30-1 | planned |
| M30-16 | `check lint`: reports definitions that cannot work: a need that names nothing, an input pattern that matches no path, a directory the tree lacks | M30-3, M30-7 | planned |
| M30-17 | Parameters: a check declares named, typed parameters with defaults that a start by hand may set; each reaches the check as an environment variable | M30-1 | planned |

#### M31: Checks run where the caller is

One host, and no store writes beyond diagnostics. It builds on M5's and M19's workspaces and on `ws
land --check`. 22 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M31-1 | `check run`: runs the checks of a stage (all of them, or a plan's once M30-4 exists) in a workspace or, in the directory form, in a directory, each with no shell and no stdin, in its own process group, under its timeout; one envelope names every check's outcome | M30-1, M30-6, M19 | in 0.13.0 |
| M31-2 | The environment a check gets: the check's name, tree, stage and attempt, the paths for its report, outputs, summary and artifacts, a file listing the changed paths of its plan, and the values it declared; nothing else is inherited unless listed; the older `check.argv` form keeps the inherited environment it has today | M31-1 | in 0.13.0 |
| M31-3 | The captured log: both streams with a time per line, and sections a check opens with a marker line; `check log --follow` on the same host reads it while it grows; the envelope keeps the existing tails | M31-1 | in 0.13.0 |
| M31-4 | Findings: a check writes path, line range, level, title and message as JSON lines to its report file; the envelope returns them, bounded, in `details.findings` | M31-2 | in 0.13.0 |
| M31-5 | Intake of test and scanner reports: a check declares a JUnit or SARIF file, and its failed cases and results become findings and counts | M31-4 | in 0.13.0 |
| M31-6 | Outputs: a check writes key and value pairs to its outputs file; a dependant reads its needs' outputs as environment variables; sizes bounded | M30-7, M31-2 | planned |
| M31-7 | `check run --jobs N`: independent checks run at once; variants honour their cap and their stop switch | M30-8, M31-1 | planned |
| M31-8 | Exclusive groups on a host: a check names a group with the scope `host`; one check of a group runs on the host at a time, the others wait up to a bound; the lock's directory is an operator's setting, so that it can span several accounts of one machine | M31-1 | planned |
| M31-9 | The stop contract: interrupt, then terminate after a grace, then kill the group; an `always` check still runs; the outcome is `cancelled`; a check that left its group is named in the outcome | M31-1 | planned |
| M31-10 | `ws land --check` runs the `before-land` plan in place of one command; the tree landed is the tree every check ran on; `check.argv` keeps working | M31-1, M30-4, M5 | planned |
| M31-11 | Run lines: each check run appends one line to a diagnostics file (beside the store, in the workspace's private state, or at a path the caller names): name, tree, stage, wait, duration, outcome, labels, peak memory; diagnostics, as the serve records are: nothing decides from them | M31-1 | in 0.13.0 |
| M31-12 | `check runs`: the run lines, and from M33-1 on the run records, which are then the source; filtered by check, tree, outcome and time, paged | M31-11 | in 0.13.0 |
| M31-13 | `check log <run>`: one check's log, or with `--failed` only the failed checks', with `--tail N` | M31-3, M31-11 | in 0.13.0 |
| M31-14 | Run again: `check run --again <run>` runs every check of a run again, `--failed` what failed and its dependants, `--check <name>` one; each is the run's next attempt, and earlier attempts stay listed | M31-12 | planned |
| M31-15 | A check marked `may-fail` reports its failure without failing the plan | M31-1 | planned |
| M31-16 | Container checks: a check names an image by digest; the host's container runtime runs it with the workspace mounted, unprivileged, with no network unless declared; Linux hosts | M31-1 | planned |
| M31-17 | Companion containers: a check declares images started before it and removed after it, reachable by name | M31-16 | planned |
| M31-18 | The summary: a check writes a bounded Markdown file that is kept with the run and read back by `check runs --summary` | M31-2 | planned |
| M31-19 | `check run --debug`: one attempt with a debug variable set and the names of its environment recorded, no values | M31-2 | planned |
| M31-20 | Shadow selection: `check run --shadow` runs every check, records beside each what the plan said, and counts as a miss a failed check the plan marked `not_applicable`; `check runs --selection` gives the runs, the would-be skips and the misses | M30-4, M31-11 | planned |
| M31-21 | Variables: settings that are not secret, per store and per target, in a file beside the store (or given by the caller in the directory form); a check reads the ones it declares; `check list` shows which | M31-2 | planned |
| M31-22 | Limits per check: memory, process count and the disk its workspace may use; a container check is held to them by the runtime; a host-run check gets the limits the platform gives without `unsafe`, and its answer lists the ones not applied | M31-1, M31-16 | planned |

#### M32: Results on a tree: the key, the required list, reuse, status, and the bridge to GitHub

It extends M7's check records and does not define them. M32-2 onward need M7-C1, and M32-7 needs
M7-C2. 15 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M32-1 | The check key and the input digest, version 1: one id from a version, the check's name, its definition digest, its input digest, its variant values, its image digest, its declared variables' values, its labels and the runner's environment stamp, under a new derived kind of the hash recipe; a changed recipe is a new kind | M30-3 | planned |
| M32-2 | `check record`: writes a result for a key as M7's Check record: a state, a summary and a report blob (findings, counts, the log's digest, timings) | M7-C1, M32-1 | planned |
| M32-3 | `check status <rev or tree>`: the roll-up for a stage and a target: each required and optional check's latest counting state, who recorded it, what it waits on and for how long, and the frame and age of the last trigger pass; no runner takes part | M32-2 | planned |
| M32-4 | `check wait <rev or tree>`: returns when every required check of the stage is final; the exit status says pass, fail or timeout; it ends with the reason `stalled` when nothing was claimed or recorded within a bound | M32-3 | planned |
| M32-5 | Reuse (issue #43): `check run`, the gate, the queue and runners take a passing record under the same key in place of a run; the plan says `reused` and names the record | M32-2 | planned |
| M32-6 | Reuse rules: a result counts only when it passed, came from a granted recorder, carries the same definition digest, is for the rule's stage and is younger than the check's maximum age | M32-5, M32-8 | planned |
| M32-7 | "Not applicable" is an answer: a required check the plan marks `not_applicable` or `reused` satisfies the gate; a required check that the candidate's file no longer defines refuses the land; nothing waits for a result that will not come | M7-C2, M32-5, M30-5, M30-11 | planned |
| M32-8 | The `check` grant: the access file says which identities may record results for which check names; through `serve` anyone else's record is refused, and a record from anyone else that reached the journal another way is kept and does not count | M32-2 | planned |
| M32-9 | Expired results: a result past its maximum age, or recorded under a definition since replaced, reads `expired`, never `passed` | M32-6 | planned |
| M32-10 | `check publish --remote <name>`: writes a tree's roll-up to GitHub as a commit status on the exported commit, under one context name; meant for a host that runs no repository code | M32-3, M13 | planned |
| M32-11 | `check fetch --remote <name> <rev>`: reads GitHub's checks and statuses for the exported commit and records them as results under a name of their own, so a store's gate can require a check GitHub ran | M32-2, M13 | planned |
| M32-12 | `check offer --remote <name>`: pushes a queue attempt's prepared changeset to a candidate branch on the remote, so that the remote's checks run on the tree the land would have, and removes the branch when the attempt ends | M22-C1, M13 | planned |
| M32-13 | Result retention: results older than a period leave the roll-ups; the records stay until M23 erases them | M32-3 | planned |
| M32-14 | Guide "Gate a land on recorded checks", which says where the gate defends and where it only guides (D12) | M32-7 | planned |
| M32-15 | The required list: a rules file beside the access file, `version = 1`: per target pattern, the names of the checks a land needs with their stage, and the longest timeout any check of that target may set; an invalid file refuses every land with its line | M14 | planned |

#### M33: Triggers and admission

What makes a run wanted, decided from the store's records before any runner exists. No process
stays: a pass is a command. 19 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M33-1 | Run records in the journal: a record kind under a feature `run`: wanted, claimed, started, finished (with its duration and attempt) and cancelled, each naming the check key, the tree, the stage and who asked; from here the journal, not the run lines, is the source for attempts, `check runs`, the cost ledger and budgets | M20, M32-1 | planned |
| M33-2 | `check start`: asks for runs of a tree and stage by hand, with the parameters a check declares and a `--key` that makes a retry a replay; returns the run ids | M33-1, M30-17 | planned |
| M33-3 | `check tick`: one pass that reads the journal from where the last pass stopped and writes the run records its events call for; idempotent, resumable by frame number; a pass that finds nothing takes no writer lock | M33-1 | planned |
| M33-4 | `check tick --follow`: the same pass in a loop, one line per pass that did something, as `derive --follow` does | M33-3 | planned |
| M33-5 | Trigger: a ref moved or was deleted, by target pattern, starts the `after-land` stage on the new head's tree | M33-3 | planned |
| M33-6 | Trigger: a tag was created, by pattern, starts the `release` stage | M33-3 | planned |
| M33-7 | Trigger: a schedule: a check names a five-field schedule and a zone; a pass starts it once per due instant on the target's head, records the instant and how late it ran; the host's own timer calls the pass, and the store host's clock decides | M33-3 | planned |
| M33-8 | Supersession: a wanted run whose candidate was replaced, or whose target moved under it, is never claimed, and a running one is stopped; the outcome is `superseded`, not a failure | M33-1 | planned |
| M33-9 | The settle window: a target or an identity may set a wait before a wanted run becomes claimable, so that a burst costs one run | M33-8 | planned |
| M33-10 | Serial groups: a check names a group with the scope `store` and what a newcomer does: wait in order, replace the one waiting, or stop the one running; the waiting line is bounded; the key is the one M31-8 reads with the scope `host` | M33-1 | planned |
| M33-11 | No loops: a land made by a run does not start that run's own check again unless the check says so; chains are bounded in depth | M33-5 | planned |
| M33-12 | Chained checks: a check names another check's final state on the same tree as its trigger | M33-3 | planned |
| M33-13 | Off and on: a switch beside the store stops a named check from being wanted, with who and why recorded; `check list` shows it | M33-3 | planned |
| M33-14 | A skip request: a land's trailer asks that its `after-land` checks not run; honoured only for granted identities, never for `before-land` | M33-5 | planned |
| M33-15 | Bounds: a cap on wanted runs per identity and per store, and on the attempts of one run; beyond it a request is refused with `E_RUN_LIMIT` and the reset | M33-1 | planned |
| M33-16 | Admission reasons: a request that was not admitted (reused, not applicable, over a bound) leaves one diagnostics line with its reason and writes no journal frame; `superseded` and `held` are states of run records; `check runs --not-admitted` reads both | M33-8, M31-11 | planned |
| M33-17 | `check cancel <run>`: stops a wanted or running run; needs a grant; a `--key` makes a retry a replay | M33-1 | planned |
| M33-18 | Lint for triggers: `check lint` reports a stage that no trigger of the store reaches | M30-16, M33-5 | planned |
| M33-19 | `check request <tree> <check>`: asks for a result again without a new changeset; needs a grant; a `--key` makes a retry a replay; the new run keeps the first requester's trust class and names who asked again | M32-2, M33-1 | planned |

#### M34: Runners that ask for work

A runner is a client. It asks a store for work, as `derive --follow` asks it for changesets, over
the native wire `serve --stdio` answers since v0.12.0. 16 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M34-1 | The claim: a compare-and-swap record with a lease measured on the store host's clock; an expired lease makes the run claimable again; a result under a lost lease is refused | M33-1 | planned |
| M34-2 | `runner run --once`: claims one wanted run its labels satisfy, writes the tree out as a sparse workspace covering the check's inputs, runs it as `check run` does, records the result, removes the workspace and exits | M34-1, M32-2, M30-9, M31-1 | planned |
| M34-3 | The check commands over the native wire: `serve --stdio` answers `check status`, `wait` and `record` with the identity of the key line; every later command marked W joins by its own unit | M32-4, M14 | planned |
| M34-4 | `runner run --follow`: stays, asks at an interval with a backoff, runs up to `--jobs` at once, and writes its own record lines; an ask that finds nothing takes no writer lock | M34-2 | planned |
| M34-5 | A runner over ssh: the claim, the tree, the log and the result travel the native wire, so a runner needs no file access to the store and no inbound port | M34-1, M34-3 | planned |
| M34-6 | The `run` grant: the access file says which identities may claim which checks on which targets, and which labels each holds; `isolated` is a label only a grant gives | M34-1 | planned |
| M34-7 | Routing: a run is claimable only by a runner holding every label its check names; a run nobody can take is reported with `E_RUNNER_NONE` after a bound, not left silent | M34-6 | planned |
| M34-8 | `runner list`: the runners seen, their labels, their last contact and what each holds | M34-4 | planned |
| M34-9 | Lease renewal and loss: a runner renews while its check runs and stops the check when renewal fails; renewals are not journal frames | M34-1 | planned |
| M34-10 | A cancel reaches the runner: a cancelled or superseded run's check is stopped by the stop contract at the runner's next renewal | M34-9, M31-9, M33-17 | planned |
| M34-11 | A runner over the HTTP entry: the same wire through the CGI program | M34-5 | planned |
| M34-12 | The operator's commands before and after every job, set on the runner, outside the repository's control | M34-2 | planned |
| M34-13 | Drain and a minimum version: on a signal a runner finishes its job and stops; a store may refuse runners older than a version, with `E_POLICY_DENIED` and the version to reach in `details` | M34-4 | planned |
| M34-14 | Unit files for `runner run --follow` and a timer for `check tick`, for systemd; a property list for launchd | M34-4, M33-3 | planned |
| M34-15 | Guide "Run checks on your own hosts" | M34-6 | planned |
| M34-16 | `runner check`: tests the store's entry, the grant, the container runtime, the clock against the store's within a bound, and that the work directory is not on the store's file system; one pass or fail line each | M34-6 | planned |

#### M35: Trust, isolation and secrets

Who may make what run where, and what a run may see. 12 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M35-1 | Trust classes: the access file sorts identities into `trusted`, `held` and `untrusted` for checks; every wanted run carries its requester's class | M33-1 | planned |
| M35-2 | Definitions a change cannot edit: for an untrusted candidate the check file is read at the target's head, not at the candidate | M35-1 | planned |
| M35-3 | Held runs: a `held` class's run waits for `check approve` by a granted identity and expires after a period, reported as expired | M35-1 | planned |
| M35-4 | The isolated pool: untrusted runs are claimable only by runners whose grant gives `isolated`: a container, no network, no secrets, no kept state; the label is not offered on macOS | M35-1, M31-16, M34-6 | planned |
| M35-5 | Secrets on the runner: a file outside every store names each secret and the checks, targets, environments and classes it is for; a check declares names; a value reaches the check only when all match, the narrowest scope winning | M34-2, M35-1 | planned |
| M35-6 | Masking: every secret value a run received is replaced in logs, findings, summaries and outputs before they leave the runner; a check may register more values | M35-5 | planned |
| M35-7 | `runner secret set`, `list`, `remove`: values go in and never come out; the list shows names, scopes and ages; each change is a line in the runner's own records (per host, not an audit record of the store) | M35-5 | planned |
| M35-8 | The check user: a runner's setting names a launcher that starts every check under another OS user, or in a container; the runner's credential and the store's directory are out of that user's reach, and `runner check` gains a line that proves it | M34-2, M34-16 | planned |
| M35-9 | Least access for a run: during a job a runner's own access to the store is read-only unless the check declares that it lands or records; `serve` enforces it from the grant | M34-6 | planned |
| M35-10 | A network list for a container check: the hosts a check may reach, where the runtime can enforce a list; where it cannot, the check is refused, not run open | M31-16 | planned |
| M35-11 | Lint for trust: `check lint` reports a check that would give a secret, kept state or write access to a class that may change the check's definition | M30-16, M35-5 | planned |
| M35-12 | Guide "Run changes you do not trust", and the matching paragraph of SECURITY.md: the program alone does not defend a host against hostile code; a machine per job behind `runner run --once` does; a candidate's own scripts still run when its definitions come from the target | M35-4 | planned |

#### M36: Logs, reports and artifacts

What a run leaves behind, and for how long. 13 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M36-1 | The log store: finished logs kept beside the store by digest, compressed, bounded per run, and synced to disk before the record that names them; a result names its log's digest | M31-3 | planned |
| M36-2 | `check log` selection: by run, check and attempt; a line range, a pattern, or every log of a run as one archive | M36-1 | planned |
| M36-3 | Artifacts: a check declares the paths to keep; the runner stores them as one tree under a name, synced before the record that names it, and the result names it | M34-2 | planned |
| M36-4 | `check artifacts list` and `get`: a run's artifacts, written out to a directory, locally and through `serve` | M36-3, M34-3 | planned |
| M36-5 | Artifacts for dependants: a check finds its needs' artifacts in a directory before it starts | M36-3, M30-7 | planned |
| M36-6 | Retention: a period per store and per artifact; `check prune` removes expired logs and artifacts, or a named run's, and records what it removed; a later read answers `expired`, `removed` or `lost`, never a bare not-found | M36-1, M36-3 | planned |
| M36-7 | Size bounds per log, per artifact and per run; past a bound the check's outcome stands and the truncation is recorded | M36-1 | planned |
| M36-8 | The build record: an artifact's record names the tree, the check key, the definition digest and the runner's identity and labels; `check artifacts verify <file>` answers from the file's digest; it shows the file is the one the store's record names, not who built it | M36-3 | planned |
| M36-9 | `check report <run> --html <dir>`: a page for people, written as files; nothing serves it | M36-2 | planned |
| M36-10 | `check badge <target>`: an image of a target's roll-up, written as a file | M32-3 | planned |
| M36-11 | `check findings <rev>`: a change's findings beside the changed lines they touch | M31-4 | planned |
| M36-12 | A section of the agents' page: "Read a failed check" | M36-11 | planned |
| M36-13 | The log while it runs, over the wire: a runner sends the log in pieces; `check log --follow` reads them from another host | M34-5, M36-1 | planned |

#### M37: The gate's rules

M7 builds the gate, and M32-15 gives it the list of required checks. These units are the rest of the
rules a trunk with many agents needs. The queue's controls are proposed slices of M22, not units
here. 18 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M37-1 | Rule sets: the rules file holds several sets per target pattern, each naming required checks with their stage and the options below; several may match, and the strictest of each rule wins | M32-15 | planned |
| M37-2 | `policy show <target>`: the rules in force for a target, merged; any reader may ask | M32-15 | planned |
| M37-3 | Evaluate mode: a rule set so marked is computed and recorded for every land and refuses none | M37-1 | planned |
| M37-4 | Bypass by grant: named identities may land past a rule, always or only through the queue; each bypass is a record with its reason; nobody bypasses without a grant | M32-15, M7-C2 | planned |
| M37-5 | `policy log`: every evaluation that refused, would have refused or was bypassed: who, which rule, which land | M37-3, M37-4 | planned |
| M37-6 | The source of a required check: a rule names the identity pattern whose result counts for it | M32-15, M32-8 | planned |
| M37-7 | Which tree a result must be for: the tree the land produces (the default before a land), or the candidate's own | M32-15, M7-C2 | planned |
| M37-8 | Patterns a target may require of a land's message, identity and source branch name; a refusal is `E_POLICY_DENIED` with the rule in `details` | M32-15 | planned |
| M37-9 | Grants to create and to delete, per ref pattern, for branches, `release/*` and tags | M32-15 | planned |
| M37-10 | A guard on the check file: a rule may require that a land which changes the check file or the owner map come from a named class of identities | M32-15, M7-C2 | planned |
| M37-11 | Intake rules on paths and sizes: a store may refuse a land or a push by path pattern, file size or path length, before any check runs; each refusal names its rule | M32-15, M14 | planned |
| M37-12 | An intake rule on credential-shaped text: a store may refuse a land or a push whose added lines match a pattern of the operator's list (a short default list ships); an identity granted the bypass passes with a recorded reason | M37-11, M37-4 | planned |
| M37-13 | A threshold on findings: a rule may refuse a land when a named check's findings reach a level | M32-15, M31-4, M7-C2 | planned |
| M37-14 | A threshold on an output: a rule may refuse a land when a named numeric output crosses a bound | M32-15, M31-6, M7-C2 | planned |
| M37-15 | A threshold against the target: a rule may refuse a land when a named numeric output moves the wrong way against the value recorded for the target's head | M37-14 | planned |
| M37-16 | History of the rules: each change of the rules file and the access file is recorded with who, when and its digest; `policy history` | M32-15 | planned |
| M37-17 | `check lint --target <target>`: reports a required name that no check of the tree defines, and a `before-land` check that no rule requires | M32-15, M30-16 | planned |
| M37-18 | Guide "Rules for a trunk that agents land on" | M37-5 | planned |

#### M38: Delivery: environments, deployments and releases

Environments in the rules file, deployment records, approvals and waits, releases with assets, and
publishing a release to GitHub. 16 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M38-1 | Environments in the rules file: a named delivery target with the refs allowed to reach it, its approvers, its wait and the scope of its secrets | M32-15 | planned |
| M38-2 | Deployment records: a record kind under a feature `deploy`: tree, ref, environment, state, a URL and the run | M20, M33-1 | planned |
| M38-3 | The `deploy` stage: a check with an environment becomes claimable only once the environment's rules pass, and its secrets are released then; an option uses the environment's scope with no record | M38-1, M38-2, M35-5 | planned |
| M38-4 | `deploy approve` and `deploy reject`, with a comment; the requester cannot approve where the rule says so; the history is kept | M38-3 | planned |
| M38-5 | The wait: a deployment becomes claimable a set time after its request, by the store host's clock | M38-3 | planned |
| M38-6 | `deploy log [environment]`: what is live where, and the history | M38-2 | planned |
| M38-7 | Replaced and temporary deployments: a newer success marks the older one of an environment inactive; a preview environment is marked temporary and can be marked removed | M38-2 | planned |
| M38-8 | Trigger: a deployment's success starts the checks that name it (a probe of what is live) | M38-2, M33-12 | planned |
| M38-9 | An outside gate: an environment may wait for a result that a named outside identity records | M38-1, M32-8 | planned |
| M38-10 | A required deployment: a rule may ask for a successful deployment record of the tree to a named environment before a land | M32-15, M38-2, M7-C2 | planned |
| M38-11 | `release create <tag>`: the immutable tag and its notes, from the changelog's section or from the lands since the last release | M6 | planned |
| M38-12 | Release assets: named artifacts kept without expiry, recorded with their digests and fetched by `release get`; an asset cannot be replaced | M38-11, M36-3 | planned |
| M38-13 | `release publish --remote <name>`: creates GitHub's release with the notes and the assets | M38-12, M13 | planned |
| M38-14 | The release gate: a tag, or a `ref txn` that moves `release/x` with it, is refused unless the tree's `release`-stage results pass | M32-15, M33-6 | planned |
| M38-15 | Guide "Take a deployment back": `revert`, then deploy the earlier tree as a new record | M38-6 | planned |
| M38-16 | Guide "Deliver from a store" | M38-13 | planned |

#### M39: Caches and set-up time

Kept directories and their scopes, kept workspaces, keyed caches, groups fitted to recorded timings,
size advice. 9 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M39-1 | Kept directories: a check names directories that outlive a run on a runner, one copy per scope of M39-9 | M34-2 | planned |
| M39-2 | A keyed cache on a runner: a key built from the blob ids of named files and from labels; an exact hit restores; a pass saves | M39-1 | planned |
| M39-3 | Fallback prefixes: ordered prefixes restore the newest entry that matches | M39-2 | planned |
| M39-4 | Eviction: a size per runner, least recently used first, and an age limit | M39-2 | planned |
| M39-5 | `runner cache list` and `remove`: keys, sizes, last use, totals | M39-2 | planned |
| M39-6 | Kept workspaces: a runner keeps a workspace per check and updates it to the next tree instead of writing every file again; a file that did not change keeps its modification time | M34-2, M19 | planned |
| M39-7 | Groups fitted to timings: `check plan` splits a variant list into N groups of near-equal recorded duration | M31-11, M30-8 | planned |
| M39-8 | Size advice: `check stats --sizing` reports each check's peak memory and processor use against its runner's size | M31-11 | planned |
| M39-9 | Scopes of kept state: per target, a `landed` copy that only runs of landed trees write, and a `candidate` copy that candidates' runs write and landed runs never read; a candidate may start from the landed copy, read-only; an untrusted run gets neither | M39-1 | planned |

#### M40: Figures, cost and events

It builds on M21's windows, rules and metrics output, and adds figures, not a second path. 12 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M40-1 | Check figures in `health`: wait before a claim, claim to start, duration, failure rate, the cancelled and superseded shares, the age of the last trigger pass and of the oldest waiting run, per check and label, in M21's windows and metric names | M21, M33-1 | planned |
| M40-2 | `check stats --per-land`: runs, reused results and runner-seconds for each landed change (the figure issue #44 measures) | M33-1 | planned |
| M40-3 | The runner-seconds ledger, from the run records: per check, identity, target and day, as JSON and CSV; seconds, no prices | M33-1 | planned |
| M40-4 | Budgets: a store may cap runner-seconds per identity or target per period; past it, runs other than `before-land` are refused with `E_RUN_LIMIT` and the reset | M40-3, M33-15 | planned |
| M40-5 | Alert rules for checks in M21's rule set: drift of the wait and of the failure rate; no trigger pass, or no claim, within a bound | M40-1 | planned |
| M40-6 | `notify --follow`: posts run, result, queue and deployment events to the subscribers a file beside the store lists, each signed with a shared key and carrying its own id, through the user's curl | M33-3 | planned |
| M40-7 | The delivery log: `notify log` and `notify resend <id>`; a failed delivery is tried again with a backoff, a bounded number of times | M40-6 | planned |
| M40-8 | `check events --after <frame>`: run, result and queue events as JSON lines, resumable, for a client that polls; a run that becomes wanted is one of them | M33-1 | planned |
| M40-9 | `check stats --flaky`: lists the checks that both passed and failed under one key | M32-2 | planned |
| M40-10 | One automatic second attempt: a rule may allow a named check one more attempt after a failure, recorded as such | M40-9, M32-15, M33-15 | planned |
| M40-11 | A recipe in the checks guide: a check that bounds the growth of tests per change, from the plan's changed paths | M30-14 | planned |
| M40-12 | Guide "What checks cost", citing the first measured figures that issue #44 records | M40-2 | planned |

#### Proposed slices of existing milestones

Work this role needs from milestones that already exist. Each row is a proposal to the owning
milestone's plan, which may accept it, change it or refuse it; until that plan answers, every unit
that names the slice waits. The `C` in an id keeps it apart from the ids those plans use.

**Proposed to M7, blame and review.**

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M7-C1 | The `check` feature is released ahead of change requests and blame: a Check record carries the check's name, the check key, the root tree it is for, a state, a one-line summary and a report blob | M20 | proposed |
| M7-C2 | The landing gate reads recorded results: a land onto a target with required checks (M32-15) is refused unless each has a counting result for the tree the land produces; the refusal names what is missing, under a code M7's plan names; the land's result names the results that admitted it | M7-C1, M32-15, M32-8 | proposed |
| M7-C3 | Trigger: a change request's new revision starts the `before-land` stage on its tree; drafts by option | M7, M33-3 | proposed |

**Proposed to M22, the landing queue.**

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M22-C1 | An attempt prepares the land: a queue attempt writes the changeset the land would produce and an attempt record, and moves no ref, so that the tree can be written out, exported and checked before anything lands | M22, M18 | proposed |
| M22-C2 | An attempt asks for its checks: the attempt and the wanted runs of its tree's `before-land` plan are one operation, so a land in the queue waits on no trigger pass; a push to `refs/land/` on a queued target enters the queue | M22-C1, M33-1, M30-4 | proposed |
| M22-C3 | Land when the results are final: an entry lands by itself once every required result for its attempt's tree counts, and leaves the queue with a reason when one fails; whoever enqueued it need not wait | M22-C1, M7-C2 | proposed |
| M22-C4 | Stacked attempts: the queue prepares each entry on the target plus the entries ahead of it; a failed entry leaves, and those behind are prepared again | M22-C1 | proposed |
| M22-C5 | Queue bounds: how many attempts are checked at once, the smallest and largest batch that lands as one `land-group`, how long to wait for a batch and for results | M22-C4 | proposed |
| M22-C6 | The batch verdict: every entry must pass, or only the batch's head; a failed batch is split until the failing entry is found | M22-C5 | proposed |
| M22-C7 | `queue jump <entry>`: puts an entry first; the entries behind it are prepared again; `queue list` gives each entry's reason for waiting | M22-C4 | proposed |

**Proposed to M11, native sync.**

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M11-C1 | Results between stores: a synced result counts on the receiving store only if that store's access file grants its recorder; wanted runs and claims do not sync | M11, M32-8 | proposed |

**Proposed to M26, an identity from a short-lived credential.**

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M26-C1 | A credential for one claim: `runner once-token` gives a machine made for one job a credential that claims exactly one run and then is void | M26, M34-6 | proposed |

#### The hosted service

A hosted service is built around the program and uses only its public surfaces: the wire, the
commands and the records. The open-source program holds everything a team needs to run checks on
its own hosts; the service holds what needs machines, money or an always-on process run for
others. A process that stays on a team's own host is the program's, and planned
([M43](#m43-a-resident-service)). The service's fourteen pieces belong to its own plan and are not
units of the program:

| Piece | What it is |
| --- | --- |
| Hosted runners | A fresh machine per job, Linux first, in set sizes |
| Pools that follow demand | Pools that grow with wanted runs and shrink to none, with a cap per account |
| More platforms | arm64 and macOS machines |
| Managed secrets | Write-only, encrypted by the client, per account and per repository |
| Job identity for clouds | Short-lived tokens that name the store, target, check and tree, which a cloud can exchange for its own credential |
| Web pages | Pages for runs, logs, the queue and deployments |
| An HTTPS API and event delivery | An HTTPS API and an always-on service that delivers events |
| Plans and metering | Prices, plans, metered storage, budgets with a hard stop, usage export |
| Account-wide rules | Rules, required checks and runner policy across an account's repositories |
| Shared storage | Shared cache and artifact storage with quotas |
| Prepared machine images | Machine images prepared ahead, with their contents published |
| Dashboards | Usage and performance dashboards |
| Notifications | Notifications by mail and chat |
| Signed build records | Build records signed by the service as the builder |

Hosted runners need a credential for one job (the proposed slice M26-C1), and M26 is deferred
until after 1.0, only when a pilot asks. So hosted runners wait on M26.

### First steps

Each step ends with something in this repository's own way of working running through SwarmOps.
Runners and jobs use a build pinned by commit that already passed the gate, so a step is in use
one land after it lands.

**Where this repository stands today**, read from its workflow files and CONTRIBUTING.md, not
measured. The gate is `just check`, started by GitHub on runners on the maintainers' host, with
build directories that persist; the `check` job of `ci.yml` has a 30-minute limit; a pull request
from a fork runs on GitHub's own machines. A rule on `stable` requires the result `tests-pass`. A
hand-made landing queue, whose code is not in the repository, pushes candidates to `feat/lq-<n>-<n>`
branches and moves `stable` after `ci.yml` is green there, so each landed change is gated twice.
`site.yml` decides its scope with `scripts/freshness.py`, and the weekly suite takes a host-wide
lock. This repository is not developed in a SwarmOps store: the repository on GitHub stays the
source of truth ([pilot.md](pilot.md)); its `.swarmops/` directory holds step 1's check file
alone, which names each check's stage since step 2: maintainers and agents start the gate with
`check run --dir . --stage before-land`, and since 0.13.0, the first release that holds
`check run`, the `check` job of `ci.yml` starts it so too, with that release's build pinned to
CD-D18's text. Since step 3 its `check` check declares the gate's JUnit report, so that `check
run`, started with nextest's `ci` profile, answers a red gate with the failed tests as findings;
CI's `check` job uploads the same report. Since step 4 a gate started with `check run` leaves a
run line for each of its two checks beside that directory's runs, or in the directory
`SWARMOPS_CHECK_RUN_LINES_DIR` names, which `check runs` and `check log --failed` read; CI's
`check` job leaves them on the runner until the runners set that variable to a directory of the
operator's, outside every checkout. No
baseline is read from the lines yet: [perf.md](perf.md) holds no figure for them.

**Two tracks.** Track A, steps 1 to 8, runs inside the jobs GitHub starts, with no store: it uses
the directory form, writes nothing to any store, needs no new record kind and freezes nothing in
the hash contract, and it is adoption, not a gate, because GitHub still triggers, reports and
decides. Track B, steps 9 to 12, is the store recording and then deciding; steps 10 to 12 wait on
questions that are open.

The first wave is twelve steps and 32 units. Every dependency of every unit in a step is an
existing milestone or a unit of an earlier step.

| Step | Track | Units | What this repository uses after it | Waits on |
| --- | --- | --- | --- | --- |
| 1 | A | M30-1, M30-2 | The repository has a check file, `.swarmops/checks.toml`, that names its gate's commands (`just check`, `just check-compare`, the lint of the workflow files, the site and server checks). `just check` runs `check list` in the directory form with the build under test, so a file that does not parse fails the gate | Nothing; starts now |
| 2 | A | M31-1, M30-6, M31-2, M31-3 | On the runners on the maintainers' host, the `check` job of `ci.yml` starts the gate with `check run --dir . --stage before-land`, using the pinned build; maintainers and agents start it the same way before pushing. GitHub is the trigger and the reporter. A fork's job keeps calling `just check` directly. The check file's timeout is the job's 30 minutes | Nothing; starts now |
| 3 | A | M31-4, M31-5 | A red gate answers with the test runner's failed cases as findings in its envelope, and an agent repairing it reads those. The existing upload of the JUnit file stays | Nothing; starts now |
| 4 | A | M31-11, M31-12, M31-13 | Every gate run on the runners leaves a run line in the operator's directory; `check runs` and `check log --failed` read them. The baseline for G27, runs and seconds per pushed commit, is read from these lines | Nothing; starts now |
| 5 | A | M30-4, M30-3, M30-5 | The `scope` job of `site.yml` and the path list of `server.yml` are answered by `check plan` in the directory form over the changed paths, and `freshness.py scope` retires from that duty. The use is advisory: it decides whether a job that is not required runs | Nothing; starts now |
| 6 | A | M30-7, M30-16 | The gate becomes several checks with declared inputs and one build check they need (repository work, not a unit); `check lint` in `just check` proves that every dependency and pattern resolves. The split gate's total time is measured against the single recipe before the split is kept; while the checks share one build directory they run one at a time | Nothing; starts now. Before it, the gate's real duration is measured |
| 7 | A | M31-20 | The gate runs with `--shadow`: every check still runs, and each run records what the plan would have skipped. This is the evidence asked for before anything is skipped | Nothing; starts now |
| 8 | A | M31-8 | The `bench-gate` and `perf-cases` jobs of `scheduled.yml` take the host's heavy lock as an exclusive group, in place of the lock taken in the workflow file | Nothing; starts now. Before it, how many machines the maintainers' runners are is settled: it decides where the lock's directory lives |
| 9 | B | none | Not a unit, and no trunk moves: the maintainers' host keeps a store that follows `stable` and the candidate branches with `so sync` and `so fetch` (M13, as it is today). Agents read from it and plan against it; nothing lands in it. The rule on `stable`, `ci.yml` and the hand-made queue are unchanged | Nothing in this plan; an operator's step |
| 10 | B | M32-11, M7-C1, M32-1, M32-2, M32-8 | A timer on the store's host runs `check fetch` for each commit the store follows: GitHub's green or red `tests-pass` becomes a recorded result on that commit's tree, written by an identity that runs no repository code. The store holds a verdict per tree. The check key's kind is frozen here | **Open:** whether M7's check record ships ahead of the rest of M7 |
| 11 | B | M32-5, M32-3, M32-6 | A tree that passed is not checked again: the `check` job of `ci.yml` first asks `check status` for the changeset its commit names, a read of the store that needs no key, and ends green from the recorded result when it counts. The second gate run of every landed change stops, and the `feat/lq-*` skip in `site.yml` and `freshness.py` is deleted. Before it, how a job on a git checkout names the changeset of its commit is settled against the code | **Open:** the same question, through step 10 |
| 12 | B | M22-C3, M22-C1, M7-C2, M32-15, M32-12, M32-4 | The trunk moves into the store, and the hand-made queue retires in the same step, so the store never holds a land its gate could not refuse. A land is a queue entry (M22's own `enqueue`); its attempt is offered to GitHub as a candidate branch under `feat/`; GitHub's runners check the tree the land will produce; `check fetch` records the verdict; the entry lands by itself, and `so sync` moves `stable` forward to the commit that was checked. An agent waits with `check wait` | **Open:** whether this repository's trunk moves into a store; whether M7's first slices ship early; the facts of the hand-made queue |

After step 8 the first half is in use with no format change and no store. After step 11 a landed
change is gated once, not twice. After step 12 the trunk, the queue and the verdicts are SwarmOps',
the runners GitHub's, and the store's gate refuses a land that has no result. Through step 12 the
gate *guides*: the rule GitHub enforces on `stable` is still what defends it.

**Wave 2: SwarmOps runners on this repository's hosts**, landed trees first and candidates last.
All of it stands behind the question of M7's first slices, and step 16 also behind the
other two.

| Step | Units | What then runs through SwarmOps here | Entry condition |
| --- | --- | --- | --- |
| 13 | M33-1, M33-2, M30-17, M30-9, M34-1, M34-2, M34-6, M34-16, M35-8, M31-9 | One runner on the store's host, as an OS user of its own with a launcher that starts checks under another, runs the weekly slow job when someone asks with `check start`. The run is of a landed tree; `scheduled.yml` still runs | The `run` feature is switched on only after every client is updated; `runner check` passes every line |
| 14 | M34-3, M34-4, M34-5, M34-7, M34-8, M34-9, M34-10, M35-9, M33-3, M33-4, M33-5, M33-11, M33-15, M33-17 | The Linux runner accounts run `runner run --follow` through `serve` with keys of their own; the `after-land` checks (the server suite, on its paths) run on them after each land. The push trigger of `server.yml` retires for trusted work | G4 and G18 measured again with run records and the runners polling; no target that was met is missed |
| 15 | M33-7, M34-14, M30-8, M31-7, M31-16, M31-22 | The weekly suite (slow cases, fuzzing, the git matrix in its pinned image, the bench gate under the exclusive group) starts from `check tick` under the host's timer. The live job against GitHub stays on GitHub: it needs a secret | None |
| 16 | M22-C2, M22-C4, M22-C5, M33-8, M33-9, M33-16, M33-19, M39-1, M39-6, M39-9, M32-7, M32-9, M30-11, M31-14, M32-10, M37-2, M37-6, M37-10, M37-17 | The gate for trusted identities runs on SwarmOps runners with no job on GitHub; `check publish` writes `tests-pass` to GitHub from the store's host. `ci.yml` remains for pull requests from forks | The gate defends on every runner; the rule on `stable` accepts a commit status as the source of `tests-pass`, or the maintainers change it; the shadow evidence exists before `not_applicable` is honoured; the gate's time on kept workspaces is measured against today's |

Also in wave 2, with no step of their own: the guides M30-14, M32-14 and M34-15, and M31-10,
`ws land --check` running the plan, which serves repositories whose gate is short. It is not this
repository's path: a gate of tens of minutes on a busy target would rebase and run again until
`--max-attempts`, the known limitation the queue removes
([known limitations](known-limitations.md#workspaces)).

**Later waves, in brief.**

| Wave | Units | What moves |
| --- | --- | --- |
| 3 | M36-1 to M36-7, M36-11 to M36-13; M35-1, M35-5 to M35-7; M34-11 to M34-13; M40-1, M40-2, M40-5, M40-8, M40-9; M30-10, M30-12, M30-13; M31-6, M31-15, M31-18, M31-21; M33-10, M33-13, M33-18 | Test reports, benchmark results and the built site become artifacts; logs are kept by digest; the site-deployment and live-run tokens become runner secrets, so the live job moves; a stopped runner or pass raises an alert; G27's figure is read from run records |
| 4 | M38-1 to M38-8, M38-15; M37-1, M37-3 to M37-5, M37-7 to M37-9, M37-11, M37-12, M37-16, M37-18; M33-6, M33-12, M33-14; M22-C6, M22-C7; M30-15, M31-17, M31-19, M32-13 | The deployment of `site.yml` and its probe of the live site become a deployment with an environment and a check after it; the meaning of the rule on `stable` is also held in the store's rules, with bypass records and intake rules |
| 5 | M38-9 to M38-14, M38-16; M36-8 to M36-10; M39-2 to M39-5, M39-7, M39-8; M40-3, M40-4, M40-6, M40-7, M40-10 to M40-12; M37-13 to M37-15; M7-C3 | The build and packaging of `release.yml` run on runners with build records, the Mac joining as a runner over ssh for the macOS build; the publish step uses `release publish`; caches, the ledger, budgets and event delivery follow |
| 6 | M35-2 to M35-4, M35-10 to M35-12; M11-C1 | Pull requests from forks can be taken off GitHub's own machines once G29 is measured and met, on machines made per job |
| 7 | M26-C1; the hosted service's pieces | A credential for one claim, and the hosted service |

**What stays on GitHub for now.**

- The public repository and its `stable` branch, as the mirror people clone (topology 2).
- The rule on `stable`. It keeps requiring `tests-pass`; GitHub's own runs feed it through step
  15, and `check publish` from step 16 on.
- All gating of candidates through step 15: the queue is SwarmOps' from step 12, the runners are
  GitHub's.
- Pull requests from forks and their checks on GitHub's own machines, so that the maintainers'
  host never runs a stranger's code, until wave 6.
- Every job that needs a secret (the live job against GitHub, the site's deployment,
  `status.yml`), until wave 3 or 4.
- Releases as the download location: the installer and `update` read GitHub's release answers.
- The dependency-update automation GitHub runs for this repository, and the lints over whatever
  workflow files remain.
- The five-minute probe of `status.yml`: a prober should not live on the host it probes.
- Issues.

### Acceptance criteria

Each runs in CI, offline. They are the exit criteria of M30 to M40; the goal rows they name are
in [Targets](#targets-3).

1. A plan is the same for the same tree and fails closed, and this repository's gate is described
   in its own check file (M30).
2. The tree landed is the tree every named check ran on, and this repository's gate starts through
   `check run` (M31).
3. Every land onto a gated target names the results that admitted it, no check runs under the
   writer lock, an unchanged key starts no run, and no result of an ungranted recorder admits a
   land (G25's counts); a check that is not applicable never leaves a gate waiting (M32).
4. No candidate replaced inside its settle window is claimed, and a request that is not admitted
   writes no frame (G26's counts); every crash, power-loss and kill sweep passes with run records
   (M33).
5. Every wanted run finishes exactly once over the kill sweep (M34).
6. Each of G29's planted attempts is refused with a named reason, and `runner check` says whether
   the gate defends (M35).
7. What a run kept reads back by digest, expires on time, and is on disk before any record names
   it (M36).
8. Every refusal and every bypass is a record (M37).
9. A secret is released only after its environment's rules pass, and a release's assets match
   their digests (M38).
10. A candidate cannot change what a trunk run restores (M39).

Four exit criteria are measurements and run outside CI: the plan's time in G26 and G4 and G18
measured again with run records (M33), G28 (M34), and G27, which is M40's whole exit criterion.

### Targets

**None is measured, and nothing here is built.** Each is a number SwarmOps sets for itself, in
[docs/goals.md](goals.md); the targets of G26 to G28 are confirmed or replaced by the first
baseline, in each milestone's plan. No section of [docs/perf.md](perf.md) holds a figure for this
role yet, and this page gives no speed or saving for it.

| Goal | Target | Measured |
| --- | --- | --- |
| G25, checks as records | Every land onto a gated target names the results that admitted it; no check runs under the writer lock; 0 runs for an unchanged key; 0 lands admitted by an ungranted recorder | Not built |
| G26, admission before a runner | 0 claims for a candidate replaced inside its settle window; 0 frames for a request that is not admitted; a plan is decided from the store alone; a plan for a change of 10 files on a tree of 50,000 in under 100 ms | Not built |
| G27, check cost per landed change | At most 1.2 runs of the `before-land` stage per landed change over a pilot week; the runner-seconds target is set from the first baseline | Not measured yet |
| G28, an agent's feedback loop | With an idle runner: wanted to started, p95 under 5 s; last result to `check wait` returning, p95 under 1 s; every failure carries a code, `retryable` and its findings; with no runner, `check wait` ends `stalled` within its bound | Not built |
| G29, untrusted changes | In the acceptance suite's planted attempts an untrusted run reaches none of: a secret, kept state, its own check definitions, the network, the runner's credential, the store's directory. A count of refused attempts, not a proof of a boundary | Not built |
| G30, the project checks itself | This repository's gate, scheduled suite and release build run through SwarmOps for trusted identities, with the gate defending; GitHub keeps pull requests from forks and the published release | Not built |

G4 (landing under a swarm's load) and G18 (short writer holds) keep their targets and are
measured again with run records and polling runners.

### Risks and open questions

Three questions are open, and the page presents what depends on them as open:

- **Whether this repository's trunk moves into a store.** Step 12 and step 16 wait on it. If the
  answer is no, steps 1 to 11 still stand: SwarmOps then starts, plans, reports and measures this
  repository's gate, records GitHub's verdicts by tree and spares the second run of each landed
  change, and decides no land. If it is yes, the entry condition is that condition 1 of
  [To usable](#to-usable) holds on this repository, or that the maintainers name this repository
  the pilot. The way back: `stable` on GitHub stays a complete mirror through `so sync`, so
  stopping the sync and landing by merge returns to today's flow.
- **Whether M7 ships its check record and gate (M7-C1, M7-C2), and M22 its proposed slices
  M22-C1 and M22-C3 on its built core, ahead of the rest of M7, M9 and M23.** Steps 10 to 16
  wait on it; steps 1 to 8 do not. If the answer is no, they wait for all of M7 and M22. Of the 181 rows of the tables above, 68
  depend on one of those four slices, directly or through other rows.
- **The facts of the hand-made landing queue this repository uses today**: what its candidate
  branch names encode, whether it batches, how long the gate takes, how many machines the runners
  are, and whether the rule on `stable` pins its required result to one source. Steps 8, 12 and 16
  wait on parts of it. If the queue batches, M22-C4 and M22-C5 move from step 16 into step 12;
  otherwise the new queue may land fewer changes a day than the one it retires.

One point waits for the maintainers: which build checks the project itself. As written here, the
project's checks are run by a build of a commit that already passed the gate on `stable`, pinned
by commit id on the runners, and not by the build under test, with two exceptions: `check list`
and `check lint` inside `just check` use the build under test, and a fork's job keeps calling
`just check` directly. A feature (`check`, `run`, `deploy`) is switched on in a store only after
every client of that store runs a build that knows it, because a store that lists a feature
answers `E_STORE_TOO_NEW` to an older build and there is no way back. Steps 2 to 8 are built to
this text until the maintainers decide.

Not measured:

- every target of G25 to G30;
- the gate's real duration (the job limit in `ci.yml` is 30 minutes; the time a run takes is not
  measured), and so every timeout the first check file sets;
- the split gate against the single recipe (step 6), and the gate's time on kept workspaces
  against today's (step 16);
- G4 and G18 with run records and polling runners;
- the cost of a plan, of a trigger pass and of an ask that finds nothing.

Not verified yet, each to be checked by whoever builds the unit that rests on it:

- whether M22's plan already holds any of the proposed slices, and how many states M7 intends for
  the Check record;
- how a job on a git checkout names the changeset of its commit (step 11);
- whether `ws create --base` accepts a changeset that no ref names (M22-C1);
- whether an offered candidate's commit id equals the landed one's (M32-12 pins it by a test);
- the `health` command that M40-1 adds figures to, and the page that M36-12 extends.

Promises that are bounded:

- The gate defends only under the three conditions of
  [How it works](#how-it-works-3); otherwise it guides. Through step 12 it guides.
- G29 counts refused attempts in a test suite; it does not prove a boundary.
- A build record shows that a file is the one the store's record names, not who built it.
- `not_applicable` satisfies a gate only after shadow evidence on that target.
- The proposed slices of M7, M22, M11 and M26 are proposals until those plans accept them.

### Non-goals

The role keeps SwarmOps' [non-goals](goals.md#non-goals). A check is not a user hook: it runs
outside every store operation and reaches the store only as a record a gate reads. The store stays
on one host; runners are clients and may be many. A runner's cache holds derived files, never
store objects, and no store reads it. A runner's checkout is a sparse workspace of real files. A
verdict that admits a land is a journal record with a full sync.

In this role SwarmOps will not be or have:

- **A workflow language.** No steps, expressions or conditions beyond "always run" inside a
  check, no packaged steps from third parties and no catalogue of them. Nothing crosses an include
  between check files but definitions.
- **A forge.** No issue tracker, package or container registry, site hosting, code scanner or
  dependency-update bot. A check may run such a tool and a deployment may publish to such a host;
  intake rules match an operator's own patterns and are not a scanning product.
- **Signed build records.** None in the program, and no separate record kind for a bill of
  materials. Signed commits are not part of this role; [M42](#m42-signed-commits) plans them.
- **A web interface in the program.** No hosted machines, web pages, HTTPS API or managed secrets:
  those are the hosted service's. `check report` writes files; nothing serves them. No second API
  beside the JSON envelope, the schema and the MCP tools, and no action buttons on a finished
  result.
- **A credential per job before M26.** Until then a runner's identity is a long-lived key,
  narrowed during a job (M35-9) and kept from the check (M35-8).

Also not in the role:

- a defence of a runner's host against hostile code by the program alone: it offers another OS
  user, a container, and a recipe for a machine per job;
- a hierarchy of organisations or accounts above a store, or a bypass that one person requests and
  another grants: a bypass is by grant, with a recorded reason;
- a trigger on the text of a comment, which would be a path for injecting instructions into
  agents, or a trigger that runs a stranger's change with a trusted run's access;
- a scaler (an operator's own may read the event feed, M40-8) or an operator for a container
  cluster; [M44](#m44-a-store-on-a-cluster)'s launcher makes one job per wanted run and is part of
  the chart, not of the program;
- a Windows runner before M12 delivers Windows, or an `isolated` label on macOS;
- an environment that comes into being because a check names it: an environment exists only in
  the rules file, which a change cannot edit;
- observation of which files a check reads: inputs are declared, and a runner's workspace holds
  only the declared inputs, so an undeclared read fails (M34-2);
- the agent's or the repository's own work: repairing a failed change so that it lands, cleaning
  up tests, choosing tools, trimming a platform's set-up, proposing dependency updates.

## Running on a cluster

**Planned: nothing in this section is built except what "What is built" names.** SwarmOps takes
on a second role beside its three topologies, and like [checks and delivery](#checks-and-delivery)
it is not a fourth one. A store runs on a cluster as a service, lands what its queue checked onto
a forge's protected branch, and signs what it lands. Until then a store reaches a forge as a plain
git remote (topology 2), the queue lands on a local store and a hand-run `so push` publishes it,
no commit that SwarmOps makes is signed, and no SwarmOps process stays between two commands.

**What is built.** The server kit and its Helm chart (`packaging/server`,
`packaging/helm/swarmops-server`) install a store's server as one pod with one volume: the pod's
sshd and web server start `swarmops serve` once per connection, and timers run the backup. The kit
is built around the program and adds no command to it. Its first installation on a cluster was on
2026-11-21 ([known limitations](known-limitations.md#the-server-kit-on-kubernetes)).

**What is settled.** The maintainers decided on 2026-11-28: the milestone ids M41 to M44; that
three non-goals are narrowed, as the table below says; and that every piece is optional, so a
store that uses none of them works as it does today. The rest of the design below is what each
milestone's plan confirms or replaces before its first unit is built. No goal number is set yet:
each milestone's plan proposes its goals, and they join [docs/goals.md](goals.md) the way that
page says goals are added.

| Until now | From this plan on |
| --- | --- |
| Signing is a non-goal: no signed commits | Signed commits are planned (M42). Signed build records stay out of the program |
| No daemon: an always-on server is a follow-up after v1, not a milestone | An optional process that stays is planned (M43). The program still needs none |
| Not a cluster | A store may run on a cluster (M44). The store still does not scale horizontally: one volume, one writer, one pod |

What does not change:

- The store stays on one host. A cluster gives it a place to run and gives it runners in number;
  it does not give it a second writer or a replica.
- The program runs with no daemon. Every command opens a store, does its work and closes it, as
  today; the service is one more way to reach a store.
- The process that answers a connection runs no command a repository defines. Checks run on
  runners, which are clients.
- A private key or a token never enters a record, a log line or an envelope.
- A land that must be signed moves no ref unless its signature is recorded with it.

### M41: A forge as the remote

Topology 2 names GitHub, but its transport is git, so any git host already works as a remote
(M13). What is planned for GitHub alone is the bridge: the three commands of M32 that exchange
candidates and verdicts. M41 gives a remote a forge kind, gives each bridge command an
implementation for Forgejo and Gitea, and takes the queue's publish (M22-R) through a forge's
branch protection. 9 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M41-1 | The forge kind: a remote in `remotes.toml` names its forge (`github`, `forgejo` or `gitea`) and the base address of its API; a remote that names none stays a plain git remote, and every bridge command refuses it with `E_INVALID_ARG` | M13 | planned |
| M41-2 | A forge credential by reference: the remote names a file or an environment variable that holds the token; the token is read at each call, sent only to the remote's own API base, and never written to a record, a log line or an envelope | M41-1 | planned |
| M41-3 | `check publish` to Forgejo and Gitea: a tree's roll-up as a commit status on the exported commit | M32-10, M41-2 | planned |
| M41-4 | `check fetch` from Forgejo and Gitea: the exported commit's statuses and its Actions runs, recorded as results under a name of their own | M32-11, M41-2 | planned |
| M41-5 | `check offer` to Forgejo and Gitea: the candidate branch, removed when the attempt ends | M32-12, M41-2 | planned |
| M41-6 | The queue publishes to a protected branch: a queued land reaches the forge as a fast-forward push by the identity the branch's rule allows; a push the forge refuses ends the entry with the forge's answer as its reason, and a remote head that moved is fetched and the entry prepared again | M22-R, M41-1 | planned |
| M41-7 | `remote request`: opens or updates the forge's pull request for a pushed branch and answers its address and number; merging and closing stay the forge's | M41-2 | planned |
| M41-8 | The bridge's tests: each forge command has an offline half against recorded answers in `just check`, and a live half that runs weekly against a Forgejo instance, as the GitHub half does against GitHub | M41-3, M41-4, M41-5 | planned |
| M41-9 | Guide "A store and Forgejo" | M41-6 | planned |

### M42: Signed commits

A commit that a store makes carries no signature, so a branch that requires signed commits
refuses it, and a deployment that verifies signatures refuses what a queue landed. Signing is done
by the user's own tool, as transport is done by the user's git: SwarmOps runs `ssh-keygen -Y sign`
or `gpg` and holds no private key. A signature is over the exported commit's bytes, and an export
must give the same commit id on every run, so a signature is made once, recorded, and written out
again. 8 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M42-1 | The signing file, beside the access file, `version = 1`: per identity, the format (`ssh` or `openpgp`) and the key's public half; per target pattern, whether a land must be signed and by whom, the lander or the store's own identity; an invalid file refuses every land onto a target it names | M14 | planned |
| M42-2 | The signature record: a `FORMAT 3` fact that holds, for one changeset, the signature over its exported commit's bytes and the signer; the export writes it as the commit's `gpgsig` header, so the same changeset exports to the same commit id on every host and in every run; an older build refuses a store that holds one with `E_STORE_TOO_NEW` | M20, M8 | planned |
| M42-3 | A land is signed before its ref moves: on a target the signing file names, `land` and the queue prepare the commit, have the signer's tool sign it outside the writer lock, and write the changeset, the record and the ref move in one journal transaction; a target that moved meanwhile makes the land prepare and sign again; a tool that fails or a key the file does not name refuses the land, and nothing moves | M42-1, M42-2, M22 | planned |
| M42-4 | Signatures and rewriting: `rebase`, `revert` and `op undo` make new changesets, which are signed when they land; a changeset already exported is never given a signature, since its commit id would change | M42-3 | planned |
| M42-5 | Reading signatures: `show` and `log` give each commit's signer and one of `good`, `bad`, `unknown_key` or `none`, checked with the user's tool against the signing file's public keys; a commit imported from git, whose `gpgsig` header the store already keeps verbatim, reads the same way | M42-1 | planned |
| M42-6 | Intake by signature: a push through `serve`, or a fetch from a remote, onto a target the signing file marks is refused with `E_POLICY_DENIED` unless every new commit on its first-parent chain is `good` for a listed signer | M42-5, M14, M13 | planned |
| M42-7 | The store signs as itself: where the signing file says so, the service signs a land with the store's key and names the lander in a trailer, so an agent needs no key of its own | M42-3, M43-1 | planned |
| M42-8 | Guide "Sign what lands"; every crash, power-loss and kill sweep also runs with signature records | M42-3 | planned |

### M43: A resident service

`swarmops serve` runs once per connection (M14). That keeps the program without a daemon, and it
leaves three things to others: something must start the queue's runner, something must start the
trigger pass, and a forge has nowhere to send an event. M43 adds one optional process for an
installation that listens and serves what the per-connection entry serves. A long-lived process
also meets a limit a short one never does: a lock wait that gives up parks a thread
([known limitations](known-limitations.md#concurrency-and-the-lock)). 10 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M43-1 | `serve --listen <address>`: one process answers the native wire and git's smart HTTP on a socket for the stores under `--root`, with the access file, the refusals and the request records of the per-connection entry; transport security is the proxy's in front of it, and it listens on loopback only unless a flag says a proxy stands in front | M14, M17 | planned |
| M43-2 | Callers of the listener: an identity comes from the proxy's header, taken only from the addresses a setting lists, or from a token whose digest the access file holds; a request with neither is `E_AUTH` | M43-1 | planned |
| M43-3 | One service per store: the service holds a lease in the store while it serves; a second service on the same store refuses to start, and commands on the store's host keep working through the writer lock | M43-1 | planned |
| M43-4 | A lock wait that gives up leaves no thread behind, so a process that lives for weeks gathers none | M17 | planned |
| M43-5 | Live, ready and drain: two addresses answer whether the process runs and whether its stores open; on a signal it finishes open requests within a bound, answers new ones with a retryable code, and exits | M43-1 | planned |
| M43-6 | The service runs the queue: it starts the runner of every target that holds an open entry, so an entry enqueued over the wire lands with no client waiting | M43-1, M22 | planned |
| M43-7 | The service runs the trigger pass and the schedules, in place of the host's timer where it is on | M43-1, M33-3 | planned |
| M43-8 | A forge's events: an address takes Forgejo's, Gitea's and GitHub's push and status events, checks each body's signature against a shared secret and its size against a bound, and starts a fetch or a `check fetch`; an event starts work and carries no verdict, and its reader gets a fuzz target | M43-1, M41-4 | planned |
| M43-9 | Figures over HTTP: the health records' counts and latencies, in the text format that monitoring systems scrape | M43-1, M21 | planned |
| M43-10 | Guide "Run a store as a service", with a unit file for systemd | M43-5 | planned |

### M44: A store on a cluster

The server kit's chart serves a store from one pod today, per connection. M44 moves the chart onto
the service and adds what a cluster has and a host has not: jobs for runners, Secrets for keys and
tokens, and volume snapshots. 9 units.

| Slice | Deliverable | Depends on | State |
| --- | --- | --- | --- |
| M44-1 | The chart runs the service: the pod's `serve --listen` takes the place of the web server's CGI entry, the probes read the service's live and ready addresses, and the stop grace period is its drain bound; sshd stays for git over ssh | M43-5 | planned |
| M44-2 | One pod is a rule: the chart has no replica count, a second pod on the same volume refuses to serve, and an update replaces the pod while clients meet a retryable refusal | M44-1, M43-3 | planned |
| M44-3 | The volume: the acceptance runs the crash and kill sweeps in a pod on a block volume, and a store on a network file system is refused in a pod as it is on a host | M44-1, M6 | planned |
| M44-4 | A runner as a job: a job template runs `runner run --once` with a work volume of its own, the `run` grant's key from a Secret, and no access to the store's volume | M34-2, M34-6 | planned |
| M44-5 | A launcher in the chart: it reads the wanted runs over the wire, makes one job for each run its labels match, up to a cap, and removes finished ones; it holds no claim and decides nothing that a store decides | M44-4, M43-1 | planned |
| M44-6 | Keys and tokens from Secrets: the signing key, the forge tokens and the event secret are mounted files that the chart never creates; a render with one missing fails and names it | M44-1, M42-1, M41-2 | planned |
| M44-7 | Backup on a cluster: the restore drill runs on a schedule beside a served release, from a volume snapshot taken while the service serves, and `verify --deep` passes on what it restored | M44-1 | planned |
| M44-8 | The cluster's tests: the chart's render and its values schema in `just check`, offline; weekly, an installation on a throwaway cluster that lands through the queue, runs a check as a job and publishes to a forge | M44-5, M41-6 | planned |
| M44-9 | Guide "Run a store on a cluster" | M44-5 | planned |

### Order and acceptance

M41, M42 and M43 do not wait for each other as wholes: a unit lands when what it depends on is in.
M41's bridge units wait for M32's, and its publish for M22-R. M42's first six units need only the
access file and `FORMAT 3`. M43's listener needs M17's serve path, whose targets are not all met
yet ([To usable](#to-usable)). M44 comes last, since it packages the other three.

What holds for every unit, as for [checks and delivery](#deliverables-3): its acceptance is a test
in `just check`, offline, and no acceptance states a speed or a duration. The units that talk to a
forge or need a cluster (M41-8, M44-8) have an offline half and a weekly live half. Every reader
of input a stranger may write (a forge's event, a forge's answer, the signing file) gets a fuzz
target. The role is done when a change that an agent enqueues on a store in a cluster lands
without a client waiting, reaches a Forgejo branch that requires signed commits as the tree its
checks ran on, and verifies there.

### Risks and open questions

- **Whether the service runs checks.** As planned it does not: it starts the queue's runner, and
  checks run on runners. The queue's runner today runs each check in a workspace on the store's
  host, so M43-6 on a cluster either keeps that or waits for M34's runners. M43's plan decides.
- **A key in a pod.** M42-7's store key is a mounted file that the signer's tool reads. Whether a
  key held outside the pod, behind an agent's socket or a signing service, is in scope is open.
- **OpenPGP or ssh first.** Both formats are named. A forge verifies either; which one M42's
  first unit builds is its plan's choice.
- **Signing and the commit's time.** A signature covers the commit's bytes, the committer's time
  among them. A land prepared again after its target moved is signed again; how often that
  happens under G4's load is not measured.
- **The listener's wire.** Smart HTTP and the native wire are named. Whether the listener also
  answers ssh, in place of the pod's sshd, is open.
- **Releases on a forge.** M38's release is published to GitHub alone (M38-13). Publishing one
  to Forgejo is not a unit of M41 yet.
- **Who answers for a hosted service's pieces.** Web pages, an HTTPS API beside the envelope and
  managed secrets stay the [hosted service's](#the-hosted-service). M43-9 serves figures and
  M43-8 takes events; neither is a second API.

### Non-goals

In this role SwarmOps will not be or have:

- **A store on several pods.** No replica, no leader election, no shared-nothing shard. Stores on
  several hosts sync as topology 3 says.
- **An operator.** No custom resource and no controller that reconciles one. The chart is
  templates, and the launcher makes jobs and nothing else.
- **A forge.** A pull request, its review and its merge button stay the forge's.
- **Transport security in the program.** A proxy in front of the listener ends TLS.
- **A key store.** SwarmOps runs the signer's tool and reads a file a Secret mounts; it makes,
  rotates and revokes no key.
- **Signed build records.** They stay out of the program, as
  [checks and delivery](#non-goals-1) says.

## Milestones

Milestone ids are never renumbered. M13 (topology 2) and M14 (topology 1) were added for the
GitHub goal, M15 to M26 by the plan of 2026-11-05, M30 to M40 on 2026-11-18 for
[checks and delivery](#checks-and-delivery), and M41 to M44 on 2026-11-28 for
[running on a cluster](#running-on-a-cluster); M11 (topology 3) takes `so clone`, `so remote` and
`so sync`.

**Settled:** M0 to M4 and M8 are delivered, in v0.1.0, M13 in v0.2.0, M5 in v0.3.0, M6 in
v0.4.0, M14 in v0.7.0, M15 in v0.8.0 and M16 in v0.9.0, ahead of M12, which is developed separately.
**Provisional:** after M16, delivered in v0.9.0, with M17's slices in v0.10.0, M18's and M19's in v0.11.0, M20's and M21's first ones in v0.12.0, M22's core in v0.13.0, and their exit criteria open, the order follows
[To usable](#to-usable): M17 serves under load, M18 and M19 need no new format, one `FORMAT 3` (M20)
then serves review, the queue, withdrawal and lineage, and topology 3 stays after the simulator.
M22's core moves ahead of M7 (2026-11-18): it needs only `FORMAT 3`. The order below is the plan:

```text
order   M13 → M5 → M6 → M14 → M15 → M16 → M17 → M18 ∥ M19 → M20 ∥ M21 → M22 → M7 ∥ M23 ∥ M22-R
        → M9 → M11 ∥ M24 → M25
        M12 apart, developed separately; M26 after 1.0, if a pilot asks
needs   M13  ← M3, M4, M8                 packs, the mirror, the sync engine
        M14  ← M13, M6                    FORMAT 2: branch names and tags
        M20  ← its design                 the manifest, record kinds
        M7   ← M20                        annotations, check results
        M22  ← M20                        queue entries, the workspace check
        M22-R ← M22, M13                  the local queue, the sync engine
        M11  ← M9, M7, M13, M14, M24      the simulator, approvals, lineage

order   checks and delivery, as planned:
        … M20 ∥ M21 ∥ M30 → M31 → M7 (its check record and gate: M7-C1, M7-C2) → M32
        → M22 (its proposed slices M22-C1, M22-C3) → M33 → M34 → M35 ∥ M36 ∥ M39 → M37 → M38 → M40
        the rest of M7 after M32; the rest of M22 with M33 and M34; M37 may start once M32 is in
needs   M30  ← M5, M18
               the workspace file, land-check
        M31  ← M5, M19, M30
               the check file and plans, workspaces
        M32  ← M7-C1, M7-C2, M13, M14, M22-C1, M30
               the check record and the gate, the attempt, the git remote, the access file
        M33  ← M20, M30, M31, M32
               record kinds, the check key, run lines
        M34  ← M14, M30, M31, M32, M33
               wanted runs, results, the entrances and their wire
        M35  ← M30, M31, M33, M34
               runners, container checks
        M36  ← M30, M31, M32, M34
               captured logs, runners, the wire
        M37  ← M7-C2, M14, M30, M31, M32
               the required list, the gate, findings and outputs
        M38  ← M6, M7-C2, M13, M20, M32, M33, M35, M36
               the required list, run records, secrets, artifacts, tags
        M39  ← M19, M30, M31, M34
               runners, workspaces, run lines
        M40  ← M21, M30, M32, M33
               health, run records, results

order   running on a cluster, as planned:
        M41 ∥ M42 ∥ M43 → M44; a unit lands when what it depends on is in
needs   M41  ← M13, M22-R, M32
               the git remote, the queue's publish, the bridge commands
        M42  ← M8, M13, M14, M20, M22, M43
               the export, the access file, FORMAT 3's facts, the queue, the service (M42-7 alone)
        M43  ← M14, M17, M21, M22, M33, M41
               the serve path, health, the queue's runner, the trigger pass, the forge kind (M43-8 alone)
        M44  ← M6, M34, M41, M42, M43
               the file system check, runners, tokens, the signing file, the service
```

The units of M30 to M40 follow their own dependencies, not whole milestones: a unit lands when
what it depends on is in, and a milestone's row turns to done when its last unit has landed. Each
`needs` line of M30 to M40 is generated from the [unit tables](#deliverables-3): it is the union
of what the milestone's units depend on outside it. Where M7's first slices and M22's proposed ones stand in
that order is not decided ([Risks and open questions](#risks-and-open-questions-3)); M30 and M31
do not depend on it and start now.

| Milestone | Topology | Deliverable | Depends on | Exit criteria |
| --- | --- | --- | --- | --- |
| ✅ M0–M4, M8 | all | The core, diff and history, git import, landing, git export; v0.1.0 | | Delivered ([CHANGELOG](../CHANGELOG.md)) |
| 📋 M12 | all | macOS and Windows builds, tests and releases; so far macOS on Apple silicon: the build, its tests, the installer and the release's archive, from v0.12.0 on | | Each platform passes `just check` in CI and ships in a release |
| ✅ M13 | 2 | The user's git as the transport; the store's git mirror; `so clone`, `so remote`, `so fetch`, `so push`, `so sync`, `so sync status`, their MCP tools; `authors.toml`; v0.2.0 | M3, M4, M8 | Topology 2's acceptance criteria 1 to 6 pass (delivered: [CHANGELOG](../CHANGELOG.md)) |
| ✅ M5 | — | Workspaces, with an adapter for Claude Code's worktree hooks; `rebase`, `land --expect-target`, `blob get`; eleven MCP workspace tools; v0.3.0 | M4 | As the README states it (delivered: [CHANGELOG](../CHANGELOG.md)) |
| ✅ M6 | 1, 3 | Verify, crash safety, fuzzing, gc, and FORMAT 2 with the ref names M11 and M14 need (remote-tracking refs, branches, tags); gc keeps what the mirror's refs reach; v0.4.0 | | As the README states it, plus a FORMAT 2 store holding a free-form branch and a tag (delivered: [CHANGELOG](../CHANGELOG.md)) |
| ✅ M14 | 1 | `swarmops serve` over ssh and CGI; upload-pack and receive-pack; `refs/land/`; the access file; v0.7.0 | M13, M6 | Topology 1's acceptance criteria 1 to 5 pass (delivered: [CHANGELOG](../CHANGELOG.md)) |
| ✅ M15 | — | Safety and contract: codes pinned, op ids, fetch integrity, `main` forward only, workspace refusals, land keys; v0.8.0 | M14 | Its twelve rows' acceptance in scope |
| ✅ M16 | — | Safety and load: no-follow workspace writes, kill tests for readers and ref moves, sync and import forward only, one lock pass per land, `ancestry`, the land key under the lock, refusal inside the quarantine; v0.9.0 | M15 | The M16 plan's acceptance, with two exceptions: `ancestry`'s p99s are not measured yet ([known limitations](known-limitations.md#commits-refs-and-history)), and five slow cases passed only when run alone ([perf.md](perf.md#m16--2026-11-07), "The slow suite"); G4 re-measured ([perf.md](perf.md#m16--2026-11-07)) |
| 📋 M17 | 1 | Serve under load: reads off the writer lock, history prepared at land and import, one pack per identical read, pushes in bounded memory, records per request, a differential suite against git's own server; its slices in v0.10.0, its closure slices in v0.11.0, not measured again | M16 | G18–G23 measured, G19, G20, G22 met. So far G18's commit wait beside an import, G19's CPU, G20's land p99 at 50 processes and G22's memory miss, and G21 has not run ([perf.md](perf.md#m17-baselines--2026-11-07)) |
| 📋 M18 | — | Take back and stay current: revert, `ref log`, the first sync after a clone, conflict attribution, group commit if needed | M16 | 55 of 55 codes pinned; first sync < 1 s on 12,000 commits. Built, in v0.11.0: `revert`, `ref log` (55 of 55 codes pinned), revisions by move and by instant, conflict reports that list every touch, `land-check`, `land-group`, `ref txn`, `merge-base`, the landing-order and undo-per-kind proofs; group commit, D2-1 to D2-7, in v0.10.0. Missed so far: the first sync after a clone, 13.081 s on 12,000 commits and 18.8× on 1,000, against 1 s and 1.5×. G4 on batching met at 50 processes in two gated runs; its 32-process runs, its slow sweeps and the quiet-host run of the sync lag (0.520×, met on a loaded host) not recorded yet ([perf.md](perf.md#d2-8-g4-on-batched-lands--2026-11-08-not-measured-the-host-was-never-quiet)) |
| 📋 M19 | — | Workspaces, second round: stale workspaces everywhere, edit snapshots, the update's latency | M16 | G9's misses met. Built, in v0.11.0: `E_WS_STALE` from every command that reads or writes a workspace, along every parent; an update that snapshots edits first and carries them; `ws snapshot`; update obstacles refused before the store; `ws status` listing what a sparse commit refuses; one scan walk on 8 threads; unreadable directories and kind changes named; a kill sweep of `ws land -m` (10 kills per format in `just check`); the OpenCode adapter; the large-tree and many-workspace cases. Not met yet: the update after a land and `ws commit` not measured again since v0.6.0's 116.6 ms and 89.0 ms (the host was never quiet), and the commit row stays missed until a change feed exists (the M19 design, decision D1); the 500-kill sweep and the new cases not recorded yet ([perf.md](perf.md#m19-fan-in--2026-11-09-not-measured)) |
| 📋 M20 | — | `FORMAT 3`: the sorted views under a manifest, the new record kinds, `upgrade` | its design | G18 at 1,000,000 records. Built, in v0.12.0: FORMAT 3 the default of `init`, its journal frames with a nonce (byte-identical operations get distinct ids), the fact records and their index behind named features, the sorted views as runs built after the lock is released, `upgrade` 2 → 3 keeping every op id, verify and repair on it, every crash, power-loss and kill sweep on all three formats, `E_STORE_TOO_NEW`, the release cut's golden check. Not measured yet: G18 at 1,000,000 records, A5's 1,000,000 commits, A1's commit cost and the 200,000-changeset upgrade ran only as a 20,000-changeset trial under load ([perf.md](perf.md#m20--format-3-at-1000000-records--2026-11-12-not-measured)) |
| 📋 M21 | — | Health, drift and housekeeping | M17 | G24: a land p99 that climbs from 50 to 120 ms is marked as drift, and a steady run is not; G24 joins [docs/goals.md](goals.md) with its first measurement |
| ✅ M22 | — | The landing queue | M20 (`FORMAT 3`) | On a local store: entries survive kill and power loss and the queue resumes (C8); candidates checked in parallel, ref moves in queue order, a red isolated by bisection (C2, C4); removal with a machine-readable reason (C3); CLI and MCP control (C9); 50 processes each with a 30 s check measured against A21's numbers, no lock spanning a check (A22). Built, in v0.13.0: `queue enqueue|status|withdraw|wait|run` and the four `queue_*` MCP tools, the kill, power-loss and SIGKILL sweeps of the runner. A22 measured: 50 of 50 landed once in 92.5 s, retries 0 %, the longest hold of the lock 72.4 ms ([perf.md](perf.md#m22--the-landing-queue-under-50-processes--2026-11-18)). Further slices proposed to its plan, not accepted yet: an attempt that prepares the land, landing when the results are final, stacked attempts and queue bounds (M22-C1 to M22-C7) |
| 📋 M7 | — | Blame and review. Proposed to its plan, not accepted yet: its check record and the gate that reads recorded results come first (M7-C1, M7-C2), a change request's new revision becomes a trigger (M7-C3), and its remaining work (change requests, approvals, blame) follows M32 | M20 (`FORMAT 3`) | G11: blame, change requests, their classification, approvals and a landing gate ([docs/goals.md](goals.md)) |
| 📋 M23 | — | Withdrawal and erasure | M20 | Every id kept |
| 📋 M22-R | 2 | The queue's remote publish (Forgejo or git remote), postland, the land-direct cutover | M22, M13 | A queued land reaches the remote's branch as the checked tree |
| 📋 M9 | — | Simulation and a benchmark gate | M20 | As the README states it |
| 📋 M11 | 3 | Native sync over `so://`, extending M13's sync engine and M14's entry. Proposed to its plan, not accepted yet: results between stores (M11-C1) | M3, M4, M6, M7, M8, M9, M13, M14, M24 | Topology 3's acceptance criteria 1 to 3 pass |
| 📋 M24 | 3 | Rewriting with lineage | M20 | Lineage kept through every rewrite and upstream land |
| 📋 M25 | 1, 2 | Moving onto a store without a pause: pushes passed on while following, archives | M17 | The switch recorded; archives byte-identical |
| 📋 M30 | — | Check definitions and plans: the check file, declared inputs, `check list`, `check plan`, stages, order, variants, parameters, `check lint`, the importer | M5, M18 | A plan is the same for the same tree and fails closed; this repository's gate is described in its own check file |
| 📋 M31 | — | Checks run where the caller is: `check run`, the environment a check gets, findings, run lines, exclusive groups, shadow selection, container checks, limits | M30, M19, M5 | The tree landed is the tree every named check ran on; this repository's gate starts through `check run` |
| 📋 M32 | 2 | Results on a tree: the check key, `check record`, the required list, reuse and its rules, `check status`, `check wait`, and the three commands that exchange candidates and verdicts with GitHub | M7 (its check record and gate), M22 (its attempt), M30, M13, M14 | G25; a check that is not applicable never leaves a gate waiting |
| 📋 M33 | — | Triggers and admission: run records, `check start`, `check tick`, schedules, supersession, bounds | M32, M31, M30, M20 | G26; every crash, power-loss and kill sweep passes with run records; G4 and G18 measured again with them |
| 📋 M34 | 1, 3 | Runners that ask for work: the claim and its lease, `runner run`, the check commands and runners through `serve`, labels and grants | M33, M32, M31, M30, M14 | Every wanted run finishes exactly once over the kill sweep; G28 measured |
| 📋 M35 | — | Trust, isolation and secrets: the check user, least access for a run, trust classes, held runs, secrets on the runner | M34, M33, M31, M30 | G29; `runner check` says whether the gate defends |
| 📋 M36 | — | Logs, reports and artifacts: the log store, artifacts, retention, build records | M34, M32, M31, M30 | What a run kept reads back by digest, expires on time, and is on disk before any record names it |
| 📋 M37 | — | The gate's rules: rule sets, evaluate mode, bypass records, intake rules, thresholds | M32, M31, M30, M14, M7 (its gate) | Every refusal and bypass is a record |
| 📋 M38 | — | Delivery: environments, deployment records, approvals and waits, releases with assets, publishing a release to GitHub | M36, M35, M33, M32, M20, M13, M6, M7 (its gate) | A secret is released only after its environment's rules pass; a release's assets match their digests |
| 📋 M39 | — | Caches and set-up time: kept directories and their scopes, kept workspaces, keyed caches, groups fitted to recorded timings, size advice | M34, M31, M30, M19 | A candidate cannot change what a trunk run restores |
| 📋 M40 | — | Figures, cost and events: check figures in health, runs per landed change, the runner-seconds ledger, budgets, event delivery | M33, M32, M30, M21 | G27 measured |
| 📋 M41 | 2 | A forge as the remote: the forge kind and its credential, the bridge commands against Forgejo and Gitea, the queue's publish to a protected branch, `remote request` | M13, M22-R, M32 | A queued land reaches a Forgejo branch that only a named identity may push, as the tree its checks ran on; every bridge command passes offline and in the weekly live run |
| 📋 M42 | — | Signed commits: the signing file, the signature record, lands signed before their ref moves, signatures read and required at intake, the store signing as itself | M20, M14, M13, M8, M22, M43 | A signed land exports to the same commit id in every run, and the forge and `git verify-commit` accept it; a land that cannot be signed moves no ref |
| 📋 M43 | 1, 3 | A resident service: `serve --listen`, its callers, one service per store, drain, the queue and the trigger pass run by the service, a forge's events, figures over HTTP | M14, M17, M21, M22, M33, M41 | An entry enqueued over the wire lands with no client waiting; every kill sweep passes against the service; a store no service serves works as before |
| 📋 M44 | — | A store on a cluster: the chart on the service, one pod as a rule, runners as jobs and their launcher, keys and tokens from Secrets, backup from a snapshot | M43, M42, M41, M34, M6 | On a throwaway cluster, weekly: a change lands through the queue, its check runs as a job, and the signed land reaches a forge |

Why this order: data safety and G4 come first (conditions 5 and 2). Serving under load follows,
because topology 1's reads queue behind lands today and its first clone is the largest measured
cost. The take-back and workspace rows need no new format and come next. One `FORMAT 3` then
serves review, the queue, withdrawal and lineage at once. Topology 3 needs the simulator, so it
stays after M9. The pilot may move any of these.

Checks and delivery (M30 to M40) stands beside that order. What it needs from M7, M22 and M11 is
proposed to those milestones' plans as slices
([Proposed slices of existing milestones](#proposed-slices-of-existing-milestones)), and their
rows above say so. M22's core is built. Whether M7's check record and gate and M22-C1 and M22-C3
ship ahead of the rest of M7 and M22 is not decided; if they do not, M32 onward wait for all of M7
and M22.

Running on a cluster (M41 to M44) stands beside both. It proposes no slice to another milestone:
its units name the units they need, and wait for them.

## Open items of the delivered and planned milestones

From the [known limitations](known-limitations.md), which give each item in full, with where it
was found.

**Delivered milestones, deferred until after the pilot.** Full fsync is slow on this host (about
21 ms for an uncontended durable commit through the binary); batching syncs, and whether any
`--durability fast` flag is justified, is M2's journal-sync decision. A burst of lands no longer retries
(v0.6.0's landing model), since M16 a land takes the lock once, and with M18's group commit (D2)
its p99 at 50 processes is under 500 ms in both gated runs; its 32-process runs are not recorded
yet. Part of each lock pass
through the binary is unexplained (M1.6's quiet-window runs, then M2's decision). M2's measured
decision keeps the commit path as it is: the index-tail scan is within its bound, and the reflog
walk, `reflog(100)` at 30.9 ms on 200,000 changesets and 154 ms on 1,000,000 against 1 ms, stays a
limitation until a `FORMAT 3` (M20) gives it a per-ref index
([perf.md](perf.md#v010--just-perf-in-a-quiet-window-2026-10-21)). Compaction rewrites both
sorted views under the writer lock (50 ms at 100,000 records, 173 ms at 1M; M3's merge outside the lock if
the stall reaches 100 ms at 100,000 records, else a `FORMAT 3`'s manifest). No test reads a real repository yet (M3's and M8's tests of
this host's repositories).

**M12.** macOS on Apple silicon builds and passes its tests, the installer takes it and the
release workflow packs it: v0.12.0 is the first release that holds the archive. Open: `just check`
on macOS in CI, a build for Intel Macs, notarization, and all of Windows. The Linux release
binaries print an `ld.so` line on glibc 2.34 to 2.38 (unscheduled).

**M13 and M14 (delivered).** Without the memo key, an export and an import onto a native head
read the whole history below it; the first sync after a clone still grows with the history, past
its target; an export writes loose
objects; an export leaves no reflog entry ([Git import and export](known-limitations.md#git-import-and-export),
[Git remotes](known-limitations.md#git-remotes)). M13 took the stale git lock files in its own
mirror, and M14 the fetch's growth (the mirror's commit-graph); the rest are open.

**M5 (delivered).** Materialised files are not synced, so a power cut can leave them torn; a
checked land takes two lock passes and may check again under a busy target; no virtual snapshot of
uncommitted edits; Claude Code removes no subagent's workspace; the workspace latencies are not
yet measured ([Workspaces](known-limitations.md#workspaces)). NFC and NFD names and the Windows
check's kill wait for M12. Since M6, `gc` deletes no changeset, so unlanded workspace heads stay.

**M6 (delivered).** Left open: the double and triple faults of the journal's failure policy that
an append-only journal cannot take back (`repair` reports them); a failed directory fsync retried,
not taken back, and ext4 without a journal, which `statfs` cannot tell from ext4; a lock that is
bounded, not first-in first-out (the M6 design, decision D3); a FORMAT 1 recovery that still walks
all of `journal.off`; `ls` decoding a wide directory's whole tree; `verify` holding every object
id in memory; objects no changeset reaches, counted but not removed (decision D5); the index's
journal reads outside the filesystem seam (M9); and the sorted views as immutable runs under a
manifest (a `FORMAT 3`, decision D2) ([Verification and repair](known-limitations.md#verification-and-repair)).

**M7.** No hooks or annotations after a land: annotations need a new record kind, so a `FORMAT 3`.

**M9.** An index of commit keys, if measurements ask for one; the benchmark gate (a 20 % regression
fails).

**M11 and M14.** `{"file": "<path>"}` content is read by whoever serves the request, and one
temp-file error names a host path: a server for other hosts refuses the first and passes on
neither.

**M30 to M40.** Until checks and delivery is built: a checked land runs one command; a check's
result is not recorded; every land's push starts a run on GitHub, even for a tree that already
passed; SwarmOps has no runner, schedule or trigger, handles no secret for a check, and keeps no
log or artifact of one ([Checks and delivery](known-limitations.md#checks-and-delivery)).

**Unscheduled**, among others: `undo` does not take back a land that later lands built on; history
follows first parents only; a page cursor does not pin the revisions it pages; a read can give up
under sustained commits; derived values crafted to agree with each other are read as valid until
`verify --deep` reports them (an authenticated cache needs a new layout version); a request takes
up to about 15 times its text in memory before an operation refuses it; `verify --deep` derives the
export oids of native changesets twice.

**Planned, not built:** signed commits ([M42](#m42-signed-commits)); until then a branch that
requires signatures refuses the store's commits.

**Not planned** ([non-goals](goals.md#non-goals)): a workflow language; a forge; signed build records in the
program ([Non-goals](#non-goals-1) of checks and delivery).

## How progress is reported

- **This page** changes with each milestone's plan (names, flags and codes fixed) and with each
  release: a milestone's row turns ✅ when its exit criteria pass in CI, and the topology sections
  move what is built from planned to "Where we are".
- **[CHANGELOG.md](../CHANGELOG.md)** records each change under **Unreleased**, and the release
  section groups them by milestone.
- **[docs/goals.md](goals.md)** rows G13, G15, G16, G17 and G25 to G30 take their measured values
  from [docs/perf.md](perf.md) as the targets above are measured.
- **The README's roadmap** and the site's roadmap page carry the milestone table and each
  topology's state: today or planned.
