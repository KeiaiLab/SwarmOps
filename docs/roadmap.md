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
rows). Topologies 1 and 3 are not served yet. v0.3.0 (M5) adds workspaces: an agent builds and
tests its change on disk and lands the tree it tested, through `so ws` or MCP, with Claude Code's
worktree hooks ([Build and test a change](operate/build-and-test.md)).

### Topology 1 today: local git, remote SwarmOps

Not served. The only server is `swarmops mcp`, for an agent on the same host, so a git client
cannot reach a store. The nearest setup is a
bare git repository beside the store, written by `git export` and served by git itself:

```sh
so git export /srv/git/acme.git --ref main --branch main   # the store's main, as git commits
# people: git clone ssh://host/srv/git/acme.git, commit, git push to a branch
so git import /srv/git/acme.git --ref feature/x --into release/x
so land release/x --key "$(uuidgen)"                      # people's work joins main
```

An import continues a branch that an export wrote, so commits people push on top of an export come
in linearly ([README](../README.md#bring-a-git-repository-in)). A person must still run the import
and the land, and a push to the branch that exports write meets `E_GIT_IMPORT_DIVERGED` or
`E_GIT_EXPORT_DIVERGED` once both sides moved
([One writer per trunk](operate/one-writer-per-trunk.md)).

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
signatures, so a branch that requires signed commits refuses the store's commits; pull requests
opened from SwarmOps; and three of the four performance targets
([Topology 2](#topology-2-local-swarmops-remote-github)). The round trip through a bare clone, with
`git import`, `git export` and a hand-run `git push`, still works
([Git round trip](guides/git-round-trip.md)).

### Topology 3 today: local SwarmOps, remote SwarmOps

Works through a git remote, one writing host at a time: the writer exports and pushes with git, the
others fetch with git and import. Two hosts that both write stop on `E_GIT_EXPORT_DIVERGED` or
`E_GIT_IMPORT_DIVERGED` rather than merge ([Git round trip](guides/git-round-trip.md#limits-to-know-first)).
Native sync is M11, planned.

### What is missing, in one table

| Needed | Topology | State on `stable` | Planned in |
| --- | --- | --- | --- |
| Packs sent and received over a connection | 1, 2 | Built for topology 2 (M13): the user's git moves them between the remote and the store's mirror; SwarmOps writes no pack, and the export writes loose objects ([known limitations](known-limitations.md#git-import-and-export)) | M13 (done), M14 |
| Git's fetch and push protocols, as a client | 2 | Built (M13), by running the user's git | M13 (done) |
| Git's fetch and push protocols, as a server | 1 | Not built | M14 |
| A server entry over ssh | 1, 3 | Not built; M11 plans `ssh <host> swarmops serve --stdio` ([architecture](architecture.md#crates-and-layering)) | M14, reused by M11 |
| Remotes, remote state and `sync` | 2, 3 | Built for git remotes (M13): `remotes.toml`, the mirror's refs, `so sync` | M13 (done), M11 (store remotes) |
| Ref names beyond `main` and `release/<name>`, and tags | 1, 3 | Built (M6): free-form branches, `tags/<path>` and `remote/<remote>/<path>` in a `FORMAT 2` store; git-side branch and tag mapping not built | M6 (done), M14 (git side) |
| Access control and identities for remote callers | 1, 3 | `E_AUTH` and `E_POLICY_DENIED` are in the schema and raised by nothing ([known limitations](known-limitations.md#serving-requests)) | M14 |
| A server that refuses `{"file": "<path>"}` content and never passes on a host path | 1, 3 | Recorded as M11's work ([known limitations](known-limitations.md#serving-requests)) | M14, M11 |

## What the three share

Four pieces serve more than one topology. Each is built once, in the first milestone that needs it.

**The store's git mirror (M13).** A bare git repository kept inside the store's directory, owned by
SwarmOps and never written by people. Fetched packs land in it; `git import`'s existing code reads
it; `git export`'s existing code writes it; what leaves for a git remote is read from it. The
import and export keep their contracts: a commit imported from git exports back as itself, byte
for byte, and the same store gives the same commits every time
([architecture, Git export](architecture.md#git-export)). The mirror holds nothing the store and
the remotes cannot give again: deleted, it is rebuilt by the next fetch and export. Its refs record
each remote's last known state (`refs/remotes/<remote>/<branch>`), so topology 2 needs no new ref
name or journal record, and waits for no new store `FORMAT`. Built in M13 (`git/mirror.git`): it
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
head, a move the op log records. Against GitHub the client does the landing; against a store the
remote does (M11).

**One server entry (M14).** `swarmops serve` runs per connection, started by ssh or by a web server
as a CGI program, so it adds no daemon. Under ssh, a key's line in the server's `authorized_keys`
runs it as a forced command, and the command the client asked for (OpenSSH passes it in
`SSH_ORIGINAL_COMMAND`) picks the protocol: `git-upload-pack` and `git-receive-pack` for git
clients, `swarmops serve --stdio` for stores. The same store answers both, so a host can serve
people with git and hosts with SwarmOps at once.

## Topology 1: local git, remote SwarmOps

### What people type

On the server, once, an operator makes a store and gives each person or tool a key line (the forced
command's flags are provisional):

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
git mirror from a committed snapshot, as `git export` does, so what a client fetches is what the
store holds and never what a crash could take back. With FORMAT 2 (M6), other branch names and
tags become store refs of their own (`branch/<name>`, `tag/<name>`, provisional), refused until
then with a per-ref `ng` and a reason the client prints.

**A push.** The server writes the received pack into a quarantine of the mirror, checks that every
object the new tips reach is present and hashes to its id, then, for each ref the client named:

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

Not measured; each becomes a `just perf` case.

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
user's keys through `ssh`. The store writes no credential to disk.

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
  names (`--token-env`); no `GH_TOKEN` or `GITHUB_TOKEN` is picked up unasked.
- **`sync` moves `main` before it pushes**, so a refused push leaves `main` landed on the remote's
  head (`details.main_moved`, and `op undo` takes it back); `sync --no-push` serves branches that
  take pull requests only. A change GitHub holds is recognised by any of three trailer lines
  (`SwarmOps-Changeset`, `SwarmOps-Squashed-From`, `SwarmOps-Land-Key`), indented or bulleted as a
  squash message lists them, or by merging as no change.
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
- Signed commits: SwarmOps has no signing, so a branch that requires signatures refuses its
  commits. Open, after M13.
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

## Milestones

Milestone ids are never renumbered. Two are new: M13 (topology 2) and M14 (topology 1); M11
(topology 3) takes `so clone`, `so remote` and `so sync`.

**Settled:** M0 to M4 and M8 are delivered, in v0.1.0, M13 in v0.2.0, M5 in v0.3.0 and M6 in
v0.4.0, ahead of M12, which is developed separately.
**Provisional:** the order after M6 is settled once the pilot has run
([README](../README.md#roadmap)); until then the order below is the plan:

```text
order   M12 → M13 → M5 → M6 → M14 → M7 → M9 → M11
needs   M13  ← M3, M4, M8                 packs, the mirror, the sync engine
        M14  ← M13, M6                    FORMAT 2: branch names and tags
        M11  ← M13, M14, M6, M7, M9       the sync engine, the server entry, the simulator
```

| Milestone | Topology | Deliverable | Depends on | Exit criteria |
| --- | --- | --- | --- | --- |
| ✅ M0–M4, M8 | all | The core, diff and history, git import, landing, git export; v0.1.0 | | Delivered ([CHANGELOG](../CHANGELOG.md)) |
| 📋 M12 | all | macOS and Windows builds, tests and releases | | Each platform passes `just check` in CI and ships in a release |
| ✅ M13 | 2 | The user's git as the transport; the store's git mirror; `so clone`, `so remote`, `so fetch`, `so push`, `so sync`, `so sync status`, their MCP tools; `authors.toml`; v0.2.0 | M3, M4, M8 | Topology 2's acceptance criteria 1 to 6 pass (delivered: [CHANGELOG](../CHANGELOG.md)) |
| ✅ M5 | — | Workspaces, with an adapter for Claude Code's worktree hooks; `rebase`, `land --expect-target`, `blob get`; eleven MCP workspace tools; v0.3.0 | M4 | As the README states it (delivered: [CHANGELOG](../CHANGELOG.md)) |
| ✅ M6 | 1, 3 | Verify, crash safety, fuzzing, gc, and FORMAT 2 with the ref names M11 and M14 need (remote-tracking refs, branches, tags); gc keeps what the mirror's refs reach; v0.4.0 | | As the README states it, plus a FORMAT 2 store holding a free-form branch and a tag (delivered: [CHANGELOG](../CHANGELOG.md)) |
| 📋 M14 | 1 | `swarmops serve` over ssh and CGI; upload-pack and receive-pack; `refs/land/`; the access file | M13, M6 | Topology 1's acceptance criteria 1 to 5 pass |
| 📋 M7 | — | Blame and review | A `FORMAT 3` (annotations, a new record kind) | As the README states it |
| 📋 M9 | — | Simulation and a benchmark gate | | As the README states it |
| 📋 M11 | 3 | Native sync over `so://`, extending M13's sync engine and M14's entry | M3, M4, M6, M7, M8, M9, M13, M14 | Topology 3's acceptance criteria 1 to 3 pass |

Why this order: topology 2 is what a team on GitHub needs first, and it needs no new `FORMAT`, so
M13 comes right after portability. Topology 1 needs branch names and tags, so M14 follows M6.
Topology 3 needs the simulator for its tests, so M11 stays last. The pilot may move any of these.

## Open items of the delivered and planned milestones

From the [known limitations](known-limitations.md), which give each item in full, with where it
was found.

**Delivered milestones, deferred until after the pilot.** Full fsync is slow on this host (about
21 ms for an uncontended durable commit through the binary); batching syncs, and whether any
`--durability fast` flag is justified, is M2's journal-sync decision. Landing retries under a burst
wait for M4's landing measurements and the landing model they choose. Part of each lock pass
through the binary is unexplained (M1.6's quiet-window runs, then M2's decision). Commit-path costs
(index tail scans, the reflog walk) wait for M2's measured decision. Compaction rewrites both
sorted views under the writer lock (M1.6's bench; M3's merge outside the lock if the stall reaches
100 ms at 100,000 records, else a `FORMAT 3`'s manifest). No test reads a real repository yet (M3's and M8's tests of
this host's repositories).

**M12.** No item; SwarmOps is Linux only, and the release binaries print an `ld.so` line on glibc
2.34 to 2.38 (unscheduled).

**M13 (delivered) and M14.** An export and an import onto a native head read the whole history
below it, and so do a fetch, a push and a sync, which missed their targets; an export writes loose
objects; an export leaves no reflog entry ([Git import and export](known-limitations.md#git-import-and-export),
[Git remotes](known-limitations.md#git-remotes)). M13 took the stale git lock files in its own
mirror; the rest are open for M14 or later.

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

**Unscheduled**, among others: `undo` does not take back a land that later lands built on; history
follows first parents only; a page cursor does not pin the revisions it pages; a read can give up
under sustained commits.

## How progress is reported

- **This page** changes with each milestone's plan (names, flags and codes fixed) and with each
  release: a milestone's row turns ✅ when its exit criteria pass in CI, and the topology sections
  move what is built from planned to "Where we are".
- **[CHANGELOG.md](../CHANGELOG.md)** records each change under **Unreleased**, and the release
  section groups them by milestone.
- **[docs/goals.md](goals.md)** rows G13, G15, G16 and G17 take their measured values from
  [docs/perf.md](perf.md) as the targets above are measured.
- **The README's roadmap** and the site's roadmap page carry the milestone table and each
  topology's state: today or planned.
