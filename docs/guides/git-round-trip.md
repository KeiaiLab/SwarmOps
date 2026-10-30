# Keep git: import, land and export

SwarmOps: a git replacement for coding-agent swarms. Git stays at both ends: `git import` brings a
repository's history into a store, agents commit and land their work there, and `git export` writes
the result back to a git branch that `git push` sends on, so that the pipeline that watches the
branch keeps running. This guide runs the round trip command by command, and lists what stops it.

**Pre-1.0** · Linux today; macOS and Windows next · MIT licence

## The round trip

Git and the store meet in one git repository on the host. Make it a bare clone of your remote:
`git import` reads it, `git export` writes it, and git itself fetches into it and pushes from it.
A store can also reach the remote itself, with no bare clone and no hand-run `git push`:
`so clone`, `so fetch`, `so push` and `so sync` ([GitHub as a remote](github-remote.md)). This
guide is the round trip without them, through a repository on the host.

```text
 remote               ~/src/acme.git        the store
 CI, deployment       a bare clone          where agents work

 ┌────────┐ git fetch ┌────────┐ git import ┌────────┐
 │ stable │ ────────▶ │ stable │ ─────────▶ │  main  │ ◀── commit, land
 │        │ ◀──────── │        │ ◀───────── │        │
 └────────┘ git push  └────────┘ git export └────────┘
```

*Where each command runs. The remote keeps its CI and deployment, and SwarmOps never talks to it.*

1. `git import` reads the repository and records its commits in the store as changesets, then moves
   `main` to the last one.
2. Agents commit in the store and `land` their changes: each one is merged three-way onto `main`.
3. `git export` writes the history behind `main` into a branch of the repository.
4. `git push` sends the branch to the remote, and the pipeline that watches it runs.

> ### In short
>
> - **What comes back is git.** A changeset imported from git exports as the commit it came from,
>   byte for byte. A changeset made in the store exports as a commit with its own author and
>   committer and a `SwarmOps-Changeset:` trailer. The same store gives the same commits every time.
> - **Nothing is merged between the two.** When git and the store both moved, the command stops with
>   exit code 3 and says why.
> - **Not yet measured on real repositories.** The tests read fixtures that real git made. A round
>   trip on real repositories, and the export's measurements, come after the pilot.

If people also push to the branch, fetch and import before agents land more on top of it, so that
the next export continues the branch. [When an export stops](#when-an-export-stops) shows what
happens otherwise.

## Before you start

- **SwarmOps, installed** (Linux today; macOS and Windows next). The
  [Quick start](https://swarmops.cc/#start) builds `swarmops` and `so`, the same program under a
  shorter name. The examples use `so`.
- **A store** on a native Linux filesystem. A path under `/mnt/` is refused with `E_UNSUPPORTED_FS`.
- **An identity for the import.** A git import, like a commit, needs one:
  `--as <family>/<instance>`, else `SWARMOPS_AGENT`. A git export needs none.
- **The repository on the same host.** Both commands read or write a path on this host (a relative
  path is taken from the working directory), and never go through a server.
- **A bare clone to meet in.** An export refuses the branch that a work tree has checked out, as
  `git push` does, and a bare repository has no work tree.

**Once, before the first import**

```sh
# where SwarmOps and git meet
git clone --bare <remote-url> ~/src/acme.git

# the store and the agent's identity, or --repo and --as on each command
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme
export SWARMOPS_AGENT=agent/worker-1

so init "$SWARMOPS_REPO" --name acme
```

## Import a git repository

`git import` reads the repository at its path, a work tree or a bare repository. Every commit that
`--ref` reaches and the store does not hold becomes a changeset, parents first, a merge with all of
its parents in their order. Then `--into` moves to the changeset of `--ref`'s commit, and is created
if it does not exist.

**Import the stable branch into main**

```text
$ so git import ~/src/acme.git --ref stable
{
  "ok": true,
  "op": "git_import",
  "api": 1,
  "result": {
    "imported": 5,
    "skipped": 0,
    "head_changeset": "8804…",
    "git_head_sha": "f686…",
    "target_ref": "main",
    "ref_moved": true,
    "lossy": [],
    "lossy_total": 0
  },
  "meta": {…}
}
```

The repository in these examples has five commits, a merge among them. The transcripts cut each id
to four digits and leave `meta` out. `skipped` counts the commits the walk found in the store
already, where it stopped; a first import skips none.

- **`--ref`**

  The commit to import, with every commit it reaches: `HEAD` by default, a ref name, short or full
  (`stable`, `refs/heads/stable`), or a commit id of 40 hex digits. A tag is peeled to its commit.
  There is no revision syntax such as `main~3`.

- **`--into`**

  The store ref to move: `main` by default, or `release/<name>`. A store has no other refs.

- **`--lossy`, `--gitlinks`**

  Allow content that a changeset cannot hold as git has it. See
  [What import refuses](#what-import-refuses).

An imported changeset is its git commit. Its author, committer and times are the commit's, and its
`git.oid` metadata holds the commit's id, which the changeset renders back to when nothing was lost.
It records no agent, so `log` shows it as `unknown/unknown`. The same git commit is the same
changeset in every store.

The full request and response types are in `so schema GitImportReq` and `so schema GitImportResp`.

## Let agents commit and land

From here the agents work in the store, as in the [Quick start](https://swarmops.cc/#start). To
publish a change, an agent commits it on the head it read, then lands it.

**An agent's change**

```text
$ so resolve main                       # the full id main holds now
$ cat > changes.json <<'EOF'
[{"path": "src/lib.rs", "op": "put",
  "content": {"inline": "pub fn f() -> u32 { 2 }\n"}}]
EOF
$ so commit -m "return two" --changes changes.json --parent <that id>
$ so land <that changeset> --key <a new uuid v7>
```

**What they print**

```text
$ so commit -m "return two" --changes changes.json --parent 8804…
{
  "ok": true,
  "op": "commit",
  "api": 1,
  "result": {
    "changeset": "ff6d…",
    "parent": "8804…",
    "root": "4eb3…",
    "files_changed": 1,
    "blobs_written": 1,
    "replayed": false
  },
  "meta": {…}
}

$ so land ff6d… --key 01a1…
{
  "ok": true,
  "op": "land",
  "api": 1,
  "result": {
    "status": "landed",
    "landed": "d2d4…",
    "rebased_onto": "8804…",
    "target": "main",
    "base": "8804…",
    "attempts": 1,
    "relocated": false,
    "files_changed": 1,
    "squashed_from": [
      "ff6d…"
    ]
  },
  "meta": {…}
}
```

`land` merges the change three-way onto the head of `main`, whatever `main` gained since, and moves
`main` to the result in one transaction, as one new changeset whose parent is that head. A land that
loses a race to another publish merges again and retries by itself. A change that conflicts with
what `main` gained fails with `E_CONFLICT` (exit 3) and the conflict report in `details.report`,
recording nothing. The same land run again with its key replays it (`status: replayed`).

`commit` and `land` are also MCP tools, so an agent can publish through `swarmops mcp`:
[Use SwarmOps from Claude Code](claude-code.md) shows how. `git import` and `git export` are not
tools; an agent runs them as commands.

The git repository changes only when an export writes it, so a landed change stays in the store
until the next export.

## Export back to git

`git export` writes the history behind a store ref (`--ref`, `main` by default) into a branch
(`--branch`, `main` by default) of the git repository at its path, and creates the branch if it does
not exist. Every commit the branch does not reach is written, parents first. The objects are made
durable before the branch moves, by compare-and-swap, and the same store gives the same commits
every time.

**Export main into the stable branch**

```text
$ so git export ~/src/acme.git --branch stable
{
  "ok": true,
  "op": "git_export",
  "api": 1,
  "result": {
    "exported": 1,
    "objects_written": 4,
    "head_changeset": "d2d4…",
    "head_sha": "a054…",
    "branch": "stable",
    "old_sha": "f686…",
    "branch_moved": true,
    "incremental": true,
    "rewritten": 0
  },
  "meta": {…}
}
```

The repository must exist already; `git init --bare` makes an empty one. It may be a bare repository
or a work tree, but a work tree's checked-out branch is refused, as `git push` refuses it, with
`E_INVALID_ARG` and `details.reason` `checked_out`. An export needs no identity.

### What git gets

- A changeset imported from git exports as the commit it came from, byte for byte.
- A changeset made in the store exports as a commit with its own author and committer and, after its
  message, a trailer block: `SwarmOps-Changeset:` with the changeset's id, then `SwarmOps-Land-Key:`
  and `SwarmOps-Squashed-From:` once a land records them.
- For a landed change, the author is the agent that made it and the committer is the lander. Both
  are named `<family>/<instance>`, with the address `<instance>@<family>.swarmops.local`. Here one
  agent did both.
- A message that already ends in a trailer block, such as `Co-Authored-By:`, shares it with
  SwarmOps' trailers.

**The exported commit, in git**

```text
$ git -C ~/src/acme.git log -1 --format=fuller
commit a054…
Author:     agent/worker-1 <worker-1@agent.swarmops.local>
AuthorDate: …
Commit:     agent/worker-1 <worker-1@agent.swarmops.local>
CommitDate: …

    return two

    SwarmOps-Changeset: d2d4…
    SwarmOps-Land-Key: 01a1…
    SwarmOps-Squashed-From: ff6d…
```

A test imports fixture repositories that real git made, lands two changes on each and exports them
into an empty repository: `git fsck --strict --no-dangling` accepts the result, and a checkout of it
gives the store's files, executable bits and symlink targets. A weekly job runs the test with git
2.39.5, 2.43.0 and 2.56.0.

Each object an export writes is a loose file, fsynced on its own, so the first export of a long
history writes one file and one fsync per object until `git gc` packs them. In a clone that already
holds the history only the commits after the branch are written: the export above wrote four
objects. An export leaves no reflog entry, so `git reflog` does not show its moves.

The request and response types are in `so schema GitExportReq` and `so schema GitExportResp`.

## Push, and CI runs

Git sends the branch on: an export writes only the repository on this host. (`so push` sends a
store ref to a remote by itself: [GitHub as a remote](github-remote.md#push).)

**Push the exported branch**

```text
$ git -C ~/src/acme.git push origin stable
To <remote-url>
   f68652e..a054ffc  stable -> stable
```

Commits imported from git come back as they were, so the branch continues what the remote has: the
push above moved the remote's `stable` forward by one commit. The roadmap moved git export ahead of
other milestones (M8) for this: work landed in SwarmOps goes back to git, and today's deployment
pipelines keep running.

Before the first push, check the remote's branch rules: a commit made in the store is not signed
(SwarmOps has no signing), and its author is an agent's name with a `swarmops.local` address.

SwarmOps has no review of its own yet: change requests, approvals and a landing gate are planned
(M7). Whatever agents land is what the next export writes. To put a person in between, export to
another branch, such as `review` (`--branch review`), and move the trunk with git after the review.

## Run it again

Both commands are incremental. Run again, `git import` takes only the commits after the one the
target's head holds, and writes nothing when that is `--ref`'s commit: it imports 0 and skips 1. Run
again, `git export` writes only what the branch's last export does not reach.

**The same export again**

```text
$ so git export ~/src/acme.git --branch stable
{
  "ok": true,
  "op": "git_export",
  "api": 1,
  "result": {
    "exported": 0,
    "objects_written": 0,
    "head_changeset": "d2d4…",
    "head_sha": "a054…",
    "branch": "stable",
    "old_sha": "a054…",
    "branch_moved": false,
    "incremental": true,
    "rewritten": 0
  },
  "meta": {…}
}
```

An import cut short by the store (a failed write, a lock held too long) says how many commits it
recorded (`details.imported`, `details.last_commit`), and the same command run again continues
after them. An import stopped by a refusal (`E_GIT_IMPORT_LOSSY`, `E_GIT_UNSUPPORTED`,
`E_GIT_READ`, `E_TOO_LARGE`, or `E_INTERNAL` for a fault of the conversion) keeps the frames it
recorded before the refused commit, 8,192 commits each, but does not count them; run again once
the cause is dealt with, it continues after them too. An `E_CAS_MISMATCH` (the target moved
after the last frame) has recorded every commit, and carries no count either.

A script can branch on the exit code: 0 for success, 3 when git and the store diverged, 4 when
running again can succeed, and 5 when an import has no identity.

<a id="stops"></a>
## When an export stops

An export stops with `E_GIT_EXPORT_DIVERGED` when git moved, and with `E_GIT_WRITE` when the
repository cannot be written.

### When git moves too

People and CI may push to the branch as well. SwarmOps does not merge their history with the
store's. An export continues a branch only when it holds the export of `--ref`'s head or of an
ancestor of it; any other branch stops the export with `E_GIT_EXPORT_DIVERGED` (exit 3).
`details.reason` says why, and the hint says what to do.

*The six reasons an export stops. The first five are found before anything is written.*

| Reason | What happened | What to do |
|---|---|---|
| **`branch_ahead`** | Commits were made in git on top of the export of the head itself. | Bring them in with `git import`, then export again. `--force` replaces them instead. |
| **`both_moved`** | Commits were made in git on top of an older export, and the source ref moved too. | Nothing is merged. `git log` shows the git side. `--force` replaces it. |
| **`branch_moved`** | The branch names a commit that is no export of this history. | `git log` shows what the branch holds. `--force` replaces it with the export. |
| **`source_moved_back`** | The source ref's head is behind the export the branch holds, after an `op undo` or a `ref set` backwards. | `so op log` names what moved the ref. `--force` moves the branch back to the export of its head. |
| **`lossy_history`** | The branch holds the original commits of a history imported lossily, which the export would replace. | Export to another branch (`--branch <name>`), or `--force` replaces the original commits. |
| **`moved_during_export`** | Another writer moved the branch while this export wrote. | Run the same export again. What it wrote stays. |

`--force` moves the branch all the same, still by compare-and-swap, replacing what git holds there.
It does not override a checked-out branch.

Take `branch_ahead`: someone merged a change in git, on top of the commit the last export pushed.
The hint names the import that brings it in, and the export after it finds the branch up to date.

**A change merged in git, brought into the store**

```text
$ git -C ~/src/acme.git fetch origin stable:stable
From <remote-url>
   a054ffc..b925315  stable     -> stable

$ so git export ~/src/acme.git --branch stable
{
  "ok": false,
  "op": "git_export",
  "api": 1,
  "error": {
    "code": "E_GIT_EXPORT_DIVERGED",
    "message": "the branch stable has diverged from d2d4… (branch_ahead): it names b925…",
    "retryable": false,
    "exit": 3,
    "details": {
      "branch": "stable",
      "exported_changeset": "d2d4…",
      "git_head": "b925…",
      "head_changeset": "d2d4…",
      "reason": "branch_ahead"
    },
    "hint": "swarmops git import /home/me/src/acme.git --ref refs/heads/stable --into main brings the commits made in git into the store, then run the export again; --force replaces them instead"
  },
  "meta": {…}
}
# exit 3

$ so git import ~/src/acme.git --ref refs/heads/stable --into main
{
  "ok": true,
  "op": "git_import",
  "api": 1,
  "result": {
    "imported": 1,
    "skipped": 1,
    "head_changeset": "10c8…",
    "git_head_sha": "b925…",
    "target_ref": "main",
    "ref_moved": true,
    "old_head": "d2d4…",
    "lossy": [],
    "lossy_total": 0
  },
  "meta": {…}
}
```

If the store moved too, the export stops on `both_moved`, and an import stops with
`E_GIT_IMPORT_DIVERGED` (exit 3): nothing is merged, and nothing is written. An import stops the
same way when the git history was rewritten, because the target's head is then not the import of an
ancestor of the branch's commit.

### When the repository cannot be written

A failed write is `E_GIT_WRITE` (exit 1), and `details.reason` says what failed. Running the export
again after the cause is gone writes what is still missing.

- **`ref_locked`**

  The branch's lock file, `refs/heads/<branch>.lock`, exists: another git process holds it, or an
  export that died left it. Remove it once no git process runs, then export again.

- **`ref_name_conflict`**

  An existing ref stands where the branch's name needs a directory, or the other way round. Export
  to another branch.

- **`io`**

  The filesystem refused a write. `details.path` names the file.

<a id="refuses"></a>
## What import refuses

Content that a changeset cannot hold as git has it is refused with `E_GIT_IMPORT_LOSSY` (exit 1),
naming the commit, the path and the class. The import stops before that commit, and the target ref
does not move.

**A submodule in the history**

```text
$ so git import ~/src/app.git --ref stable
{
  "ok": false,
  "op": "git_import",
  "api": 1,
  "error": {
    "code": "E_GIT_IMPORT_LOSSY",
    "message": "commit 6293… holds gitlinks content at vendor/lib: a gitlink, to commit 1111…; allowing the class drops or normalizes it",
    "retryable": false,
    "exit": 1,
    "details": {
      "class": "gitlinks",
      "commit": "6293…",
      "path": "vendor/lib"
    },
    "hint": "run it again with --lossy gitlinks to drop or normalize it and mark the changeset (git.lossy)"
  },
  "meta": {…}
}
# exit 1
```

`--lossy <class>` (repeatable, or `all`) drops or normalizes that class instead, and the changeset
names it in its `git.lossy` metadata. `--gitlinks drop` is `--lossy gitlinks`, and `--gitlinks`,
when given, decides that class whatever `--lossy` says. The result lists the first 100 commits taken
lossily, and `lossy_total` counts them all.

*The six classes, and what an import does to each when it is allowed.*

| Class | Git content | With the class allowed |
|---|---|---|
| **`gitlinks`** | A submodule's entry. | Dropped. |
| **`paths`** | A name that is no path component: not UTF-8, `.`, `..`, empty, or holding a `/`. | Dropped. |
| **`modes`** | A mode git does not write, such as `100664` or a zero-padded `040000`. | Normalized: a regular file becomes a file or an executable by its owner's execute bit, and `040000` a directory. |
| **`tree_order`** | A tree whose entries are not in git's order, or that names one entry twice. | Put in order, the first of a name kept. |
| **`empty_trees`** | An entry naming an empty tree (an empty directory). | Dropped. |
| **`nonstandard`** | A commit whose bytes its fields cannot give back: a signature line, time or header line that renders back differently, no blank line before the message, or a parent listed twice. | Kept as its fields hold it; a parent listed twice is kept once. |

Some repositories are not read at all. A SHA-256 repository, reftable refs and a shallow clone are
refused with `E_GIT_UNSUPPORTED`, and so are a commit of more than 255 parents and an entry whose
mode is no kind of entry. An object the repository lacks, as a partial clone lacks blobs, is
`E_GIT_READ`.

> ### A lossy import changes what comes back
>
> - A commit the import took with a loss renders to other bytes than the commit it came from, and so
>   does every commit above it, whose parent's id changes with it.
> - They export as new commits (`rewritten` counts them, with a warning), which a branch built on
>   the originals does not take (`lossy_history`), and the signature headers they keep verbatim no
>   longer verify.

## Limits to know first

- **One writing host at a time.** Between hosts SwarmOps syncs through a git remote: the writer
  exports, then pushes with git, and the others fetch with git, then import. Two hosts that both
  write stop on `E_GIT_EXPORT_DIVERGED` or `E_GIT_IMPORT_DIVERGED` rather than merge. With a remote
  that each store reaches itself, `so sync` lands one host's changes on top of the other's
  ([GitHub as a remote](github-remote.md#sync)); offline-first sync between stores is planned (M11).
- **Linux today.** `git import` and `git export` read and write a repository on this host, and
  SwarmOps is built and tested on x86_64 Linux.
- **Branches, not tags.** A store has the refs `main` and `release/<name>`, no tags, and an export
  moves one branch. SwarmOps has no user hooks, signing, submodules or LFS.
- **The export is a contract.** The rendering of commits made in the store is version 1 of the
  `git_oid` derivation. A change to it is a new version, never in place, and every branch exported
  before it meets it as divergence, to be exported again once with `--force`: a deployment pipeline
  sees a rewritten history.
- **Pre-1.0.** A minor version may break a public surface other than the hash contract, and stores
  are not migrated between pre-release formats: a store made before a format change may be refused
  afterwards and need creating again.
- **No review yet.** Change requests, approvals and a landing gate are planned (M7).
- **Not yet measured.** The tests read fixtures that real git made; a round trip on real
  repositories and the export's measurements come after the pilot. No git baseline has been
  measured, and landing is not measured under a swarm's load.
- **An export queues behind the swarm.** It reads a committed snapshot under the store's writer
  lock, so it waits behind committing agents as a writer does, and past the store's lock timeout it
  fails with the retryable `E_BUSY` (exit 4).
- **A crash can leave a lock.** An export that dies between taking `refs/heads/<branch>.lock` and
  renaming it leaves the file, as git does. Every later export of that branch, and git itself,
  refuses the branch until the file is removed by hand, once no git process runs.

## Go further

- The README's [Bring a git repository in](../../README.md#bring-a-git-repository-in) and
  [Send the work back to git](../../README.md#send-the-work-back-to-git) have the full text of both
  commands.
- [Git import](../architecture.md#git-import) and [Git export](../architecture.md#git-export) in the
  architecture overview walk through each step the commands take.
- [Known limitations](../known-limitations.md#git-import-and-export) lists what is still open for
  import and export, and [versioning and stability](../policies/versioning-and-stability.md) says
  what stays compatible.
- [Use SwarmOps from Claude Code](claude-code.md) registers `swarmops mcp`, and
  [Parallel coding agents on one repository](parallel-agents.md) sets SwarmOps beside git worktrees.
- The [home page](https://swarmops.cc/) has the comparison with git and the roadmap.
