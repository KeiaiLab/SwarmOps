# The daily loop

The loop keeps two copies of the history in step: the store, where agents commit and land, and
the git repository that people, CI and deployment use. The store reaches that repository as a
git remote: `swarmops fetch`, `push` and `sync` run your own git (2.39 or later) against it, with
your credentials, and keep what they fetched in a git mirror inside the store. The examples call
the repository `acme`, its trunk `stable`, and its remote `git@github.com:acme/acme.git` (a
stand-in: the runs behind this guide used a local repository served over a stand-in for ssh).
[GitHub as a remote](../guides/github-remote.md) is the guide to the commands themselves.

Until a review is part of SwarmOps (M7), a person reviews what agents landed before it reaches a
trunk that deploys. Pushes therefore go to a *review branch*, `review` in the examples, where CI
and the reviewer see them, and the trunk moves when the pull request from it is merged
([step 5](#5-review-then-move-the-trunk)). Nothing in SwarmOps stops a push to the trunk
(`swarmops sync origin` makes one); the loop below never makes one.

Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

```text
  remote (CI, deployment)                                     store
  -----------------------                                     -----
  stable ---- swarmops sync origin --no-push ------------->  main  <-- agents commit and land
  review <--- swarmops push origin main:review -----------  main
  stable <--- the pull request from review, merged by any method, after green CI
```

## Before you start

- An identity of your own ([Stores](stores.md)), and agents that reach the store
  ([Give agents the store](agents.md)).
- git 2.39 or later on the host, with access to the remote (an ssh key, or a credential helper or
  a token for HTTPS: [GitHub as a remote](../guides/github-remote.md#credentials)), and `python3`
  for the `field` helper of step 2.
- CI that runs on a push of the review branch ([step 4](#4-send-it-to-git-for-review)).

## Once: bring the repository in

```sh
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme
export SWARMOPS_AGENT=operator/ada
swarmops clone git@github.com:acme/acme.git --into "$SWARMOPS_REPO" --branch stable
```

```json
{"ok":true,"op":"clone_repo","api":1,"result":{"repo":{"repo_id":"3930dfc5…","name":"acme","main":"c51b1735…","store_version":2,"changesets":3,"refs":1},"remote":{"name":"origin","url":"git@github.com:acme/acme.git","branch":"stable","lossy":[]},"fetch":{"remote":"origin","branch":"stable","head":{"sha":"a4a47ecd332f37219c9e1c12ddf79977cd168f46","changeset":"c51b1735…"},"imported":3,"branches":[{"branch":"stable","new":"a4a47ecd332f37219c9e1c12ddf79977cd168f46"}],"branches_truncated":false,"tags_fetched":0},"imported":3,"main":"c51b1735…"},"meta":{"repo":"…","elapsed_ms":235,…}}
```

`clone` made the store, recorded the remote as `origin` following `stable` (`--branch`; without
it, the remote's default branch), fetched it into the store's mirror, and imported the trunk's
history into `main`, each changeset the git commit it came from. `imported` counts the commits
that became changesets. The remote's other branches are in the mirror (`branches`) but not
imported; its tags are fetched and never imported. Content a changeset cannot hold as git has it
(a submodule, a name that is not UTF-8, a mode or a tree order git does not write, an empty
directory, a commit whose bytes its fields cannot give back) is refused with
`E_GIT_IMPORT_LOSSY`, naming the commit, the path and the class, until you allow that class with
`--lossy` ([README](../../README.md#bring-a-git-repository-in)). SHA-256 repositories are refused.
A clone that stops half-way continues when run again with the same URL into the same directory.

## 1. Take in what git gained

```sh
swarmops sync origin --no-push
```

```json
{"ok":true,"op":"sync","api":1,"result":{"remote":"origin","branch":"stable","status":"up_to_date","attempts":1,"published":[],"dropped":[],"main":{"old":"c51b1735…","new":"c51b1735…"},"remote_head":{"old":"a4a47ecd332f37219c9e1c12ddf79977cd168f46","new":"a4a47ecd332f37219c9e1c12ddf79977cd168f46"}},"meta":{"repo":"…","elapsed_ms":8,…}}
```

`sync` fetches, then puts `main` on top of the trunk's head: with nothing new on either side it
moves nothing (`up_to_date`). Commits pushed to the trunk come in and `main` moves to the last of
them (`fast_forwarded`); lands that the trunk does not hold yet are landed again on top of its
head, one by one, as `land` merges them (`landed_locally`, each in `published`). `--no-push`
keeps it from pushing: the trunk takes pull requests only. `swarmops fetch origin` alone takes the
commits into the store and moves no ref; `swarmops sync status origin` then counts what `main` has
that the trunk lacks (`ahead`) and the reverse (`behind`), offline.

## 2. Agents commit and land

Agents do this through the `commit` and `land` tools. The same requests at the command line, as an
operator or a script makes them, read `main`'s head, commit a change on it, and land it. `field`
prints one field of an envelope:

```sh
field() { python3 -I -c 'import functools, json, sys; print(functools.reduce(lambda v, k: v[k], sys.argv[1].split("."), json.load(sys.stdin)))' "$1"; }
BASE=$(swarmops resolve main | field result.id)
cat change.json
swarmops --as agent/worker-1 commit -m "greet the swarm" --changes change.json --parent "$BASE"
swarmops --as agent/worker-1 land 994a4ed3f65e5fc7765bd74e9b27c1138a86c98e06be58c5432ebb1dca6a11a9 --key "$(uuidgen)"
```

```text
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn greet() -> &'static str {\n    \"hello, swarm\"\n}\n"}}]
{"ok":true,"op":"commit","api":1,"result":{"changeset":"994a4ed3f65e5fc7765bd74e9b27c1138a86c98e06be58c5432ebb1dca6a11a9","parent":"c51b1735…","root":"3d9867ea…","files_changed":1,"blobs_written":1,"replayed":false},"meta":{"repo":"…","elapsed_ms":21,…}}
{"ok":true,"op":"land","api":1,"result":{"status":"landed","landed":"91073fdd7c734576c4aa0e08f0c39800aa26d2b957eab71b5c24b0e1fdf30b0e","rebased_onto":"c51b1735…","target":"main","base":"c51b1735…","attempts":1,"relocated":false,"files_changed":1,"squashed_from":["994a4ed3…"]},"meta":{"repo":"…","elapsed_ms":25,…}}
```

The commit records the change without moving any ref; the land merges it onto `main`'s head
three-way and moves `main` to the result in one transaction, merging again by itself when `main`
moved meanwhile. The `--key` (here a random uuid, which `land` takes: 1 to 64 letters, digits and
`-`) makes a retry of the same land a replay instead of a second land.

A second agent changed the same line on the same base. Its land merges onto the first agent's
result, meets the conflict, and records nothing:

```sh
cat change2.json
C2=$(swarmops --as agent/worker-2 commit -m "a shorter greeting" --changes change2.json --parent "$BASE" | field result.changeset)
swarmops --as agent/worker-2 land "$C2" --key "$(uuidgen)" --pretty
```

```text
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn greet() -> &'static str {\n    \"hi\"\n}\n"}},
 {"path": "README.md", "op": "put", "content": {"inline": "# acme\n\nA small service that greets, briefly.\n"}}]
{
  "ok": false,
  "op": "land",
  "api": 1,
  "error": {
    "code": "E_CONFLICT",
    "message": "1 path(s) of the change conflict with the target's head 91073fdd7c734576c4aa0e08f0c39800aa26d2b957eab71b5c24b0e1fdf30b0e: nothing was recorded",
    "retryable": false,
    "exit": 3,
    "details": {
      "report": {
        "base": "c51b1735c428042e4ab5920448064b62a15b1310fc14ce2b7ea7d8eb38ef2767",
        "files": [
          {
            "base_blob": "2cda0801…",
            "hunks": [
              {
                "base": {
                  "first": 2,
                  "last": 2,
                  "text": "    \"hello\"\n"
                },
                "id": 0,
                "ours": {
                  "first": 2,
                  "last": 2,
                  "text": "    \"hi\"\n"
                },
                "theirs": {
                  "first": 2,
                  "last": 2,
                  "text": "    \"hello, swarm\"\n"
                }
              }
            ],
            "kind": "content",
            "ours_blob": "a3aeb84a…",
            "path": "src/lib.rs",
            "theirs_agent": "agent/worker-1",
            "theirs_blob": "5d417d12…",
            "theirs_changeset": "91073fdd7c734576c4aa0e08f0c39800aa26d2b957eab71b5c24b0e1fdf30b0e"
          }
        ],
        "kinds": [
          "content"
        ],
        "ours": "7017711372805e975731cd46b175a3766b0dd342511a3142cfae15a426e8c26e",
        "theirs": "91073fdd7c734576c4aa0e08f0c39800aa26d2b957eab71b5c24b0e1fdf30b0e",
        "truncated": false
      }
    },
    "hint": "swarmops cat 91073fdd…:src/lib.rs, then swarmops land 70177113… --resolve <file> (resolutions over MCP) with each path's resolution and against = 91073fdd…"
  },
  …
}
```

`README.md`, which only the second agent changed, would apply as it is; `src/lib.rs`, whose same
line both changed, conflicts. The report gives each side's lines, numbered as `cat --lines` takes
them, and the agent that last changed the path on `main`.

The agent resolves it without a checkout, against the report's `theirs`, and lands again:

```sh
cat resolve.json
swarmops --as agent/worker-2 land "$C2" --resolve resolve.json --key "$(uuidgen)"
```

```text
{"against": "91073fdd7c734576c4aa0e08f0c39800aa26d2b957eab71b5c24b0e1fdf30b0e",
 "files": {"src/lib.rs": {"content": {"inline": "pub fn greet() -> &'static str {\n    \"hi, swarm\"\n}\n"}}}}
{"ok":true,"op":"land","api":1,"result":{"status":"landed","landed":"8dc0ca37d8a4a5b29d0e95892066be702aee4b4f99cc699115cb2e8ef12293c6","rebased_onto":"91073fdd…","target":"main","base":"c51b1735…","attempts":1,"relocated":false,"files_changed":2,"squashed_from":["70177113…"]},"meta":{"repo":"…","elapsed_ms":26,…}}
```

Through MCP, the `land` tool takes the same resolutions as its `resolutions` argument.

## 3. Look at what landed

```sh
swarmops log -n 3 --human
swarmops diff "$BASE" main --format stat --human
swarmops op log -n 2 --human
```

```text
entries[0].id: 8dc0ca37d8a4a5b29d0e95892066be702aee4b4f99cc699115cb2e8ef12293c6
entries[0].parents[0]: 91073fdd7c734576c4aa0e08f0c39800aa26d2b957eab71b5c24b0e1fdf30b0e
entries[0].author_agent: agent/worker-2
entries[0].created_at: 1792671661000
entries[0].title: a shorter greeting
…
from: c51b1735c428042e4ab5920448064b62a15b1310fc14ce2b7ea7d8eb38ef2767
to: 8dc0ca37d8a4a5b29d0e95892066be702aee4b4f99cc699115cb2e8ef12293c6
files[0].path: README.md
files[0].status: modified
files[0].binary: false
files[0].insertions: 1
files[0].deletions: 1
files[1].path: src/lib.rs
…
ops[0].op: 5
ops[0].kind: land
ops[0].changesets[0].id: 8dc0ca37d8a4a5b29d0e95892066be702aee4b4f99cc699115cb2e8ef12293c6
ops[0].changesets[0].agent: agent/worker-2
…
ops[0].ref_moves[0].name: main
ops[0].ref_moves[0].old: 91073fdd7c734576c4aa0e08f0c39800aa26d2b957eab71b5c24b0e1fdf30b0e
ops[0].ref_moves[0].new: 8dc0ca37d8a4a5b29d0e95892066be702aee4b4f99cc699115cb2e8ef12293c6
ops[0].ref_moves[0].actor: agent/worker-2
…
```

`op log` lists every write transaction with its id; `swarmops op undo <id>` moves the refs one of
them moved back, by compare-and-swap, all or none, and `main` only forward: the undo of this land
is refused (`E_REF_PROTECTED`), and a change that reverts it is landed instead
([README](../../README.md#first-commands)).

## 4. Send it to git for review

```sh
swarmops push origin main:review
```

```json
{"ok":true,"op":"push","api":1,"result":{"remote":"origin","branch":"review","status":"created","new":"4db8e624d22fd98a3ca1dc51c3622c313ce7d64c","head_changeset":"8dc0ca37…","objects_written":9,"rewritten":0},"meta":{"repo":"…","elapsed_ms":69,…}}
```

The push exports every changeset the branch does not reach as a git commit into the store's
mirror, then has git send it with a lease on the value the last fetch saw: the remote branch moves
only from there to a descendant of it, never by force. This one made the branch (`created`); later
ones move it forward (`pushed`, with `old`), and one with nothing new sends nothing and starts no
git (`up_to_date`). A branch someone moved since the last fetch fails with `E_REMOTE_DIVERGED`
(exit 3) and nothing is sent: fetch, then push again, or sync. CI runs on the push as it runs on any
branch push (point it at this branch if it runs only on the trunk or on pull requests), and the
reviewer reads the branch. What they see, in a clone of their own:

```sh
git log --format="%h %an <%ae>%n    %s" -3 origin/review
git log -1 --format=%B origin/review
```

```text
4db8e62 agent/worker-2 <worker-2@agent.swarmops.local>
    a shorter greeting
91fc10d agent/worker-1 <worker-1@agent.swarmops.local>
    greet the swarm
a4a47ec Ada Lovelace <ada@example.com>
    docs: say what acme does
a shorter greeting

SwarmOps-Changeset: 8dc0ca37d8a4a5b29d0e95892066be702aee4b4f99cc699115cb2e8ef12293c6
SwarmOps-Land-Key: 23fc28da-8ee7-4ed6-ba30-b94291a54525
SwarmOps-Squashed-From: 7017711372805e975731cd46b175a3766b0dd342511a3142cfae15a426e8c26e
```

Commits fetched from git go back as themselves, byte for byte. A changeset made in the store
becomes a commit whose author and committer are the identities the store recorded for it (an
agent's, with a `swarmops.local` address, unless the store's `authors.toml` gives the agent's
family an address of its own: [Commit authors](../guides/github-remote.md#commit-authors)), with
SwarmOps' trailers after the message and no signature: a pipeline or branch rule that requires
signed commits refuses it. A message that already ends in a trailer block, such as
`Co-Authored-By:`, keeps it, and the SwarmOps trailers join that block. Tags stay git's: tag a
pushed commit in git, as before.

## 5. Review, then move the trunk

A person reads what the review branch has that the trunk lacks (a pull request from `review` to
`stable`), and CI runs on the branch. When the review approves and CI is green, the pull request
is merged by any of the host's methods: a merge commit, a squash or a rebase. Then the store
takes the trunk in:

```sh
swarmops sync origin --no-push --pretty
```

```text
{
  "ok": true,
  "op": "sync",
  "api": 1,
  "result": {
    "remote": "origin",
    "branch": "stable",
    "status": "fast_forwarded",
    "attempts": 1,
    "published": [],
    "dropped": [
      {
        "changeset": "91073fdd7c734576c4aa0e08f0c39800aa26d2b957eab71b5c24b0e1fdf30b0e",
        "by": "trailer"
      },
      {
        "changeset": "8dc0ca37d8a4a5b29d0e95892066be702aee4b4f99cc699115cb2e8ef12293c6",
        "by": "trailer"
      }
    ],
    "main": {
      "old": "8dc0ca37d8a4a5b29d0e95892066be702aee4b4f99cc699115cb2e8ef12293c6",
      "new": "d29be6aada952285d7243ba06651a73f320bce2b08a6317bd4b493ca212abfb3"
    },
    "remote_head": {
      "old": "d068fccdd8df508f9d5c025f05679e2c967c2e8c",
      "new": "d068fccdd8df508f9d5c025f05679e2c967c2e8c"
    }
  },
  …
}
```

This pull request was squashed: the trunk gained one commit that the store never exported, whose
message lists the two commits with their `SwarmOps-Changeset:` lines, as GitHub's default squash
message does. The sync recognised both changes by those lines (`by: "trailer"`), dropped them
rather than land them twice, and moved `main` to the squash commit's changeset. A rebase keeps the
lines too; a merge commit needs none, since the trunk then holds the exported commits themselves.
A squash whose message lost the lines is recognised when what is left of the store's changes
merges onto the trunk's head as no change (`by: "no_op"`). A dropped change stays in the store,
readable by id; an undo of the sync's move of `main` is refused (`E_REF_PROTECTED`), so a change
that restores it is landed instead.

When agents landed more while the review ran, the same sync lands those changes on top of the
trunk's head (`landed_locally`), and the next `swarmops push origin main:review` sends them for the
next review.

Next: an agent that must build or test before it lands works in a
[git worktree](build-and-test.md); [One writer per trunk](one-writer-per-trunk.md) is the rule for
the round trip through a bare clone, below.

## Without a remote: a bare clone

`git import` and `git export` still meet a git repository on the host, a bare clone beside the
store, the *git side*, when the store should not reach the remote itself, or when another page
needs a git repository of the store's work ([Build and test a change](build-and-test.md),
[One writer per trunk](one-writer-per-trunk.md)):

```sh
GIT_SIDE=~/.local/state/swarmops/git/acme.git
git clone --bare git@github.com:acme/acme.git "$GIT_SIDE"
swarmops git import "$GIT_SIDE" --ref stable --into main     # take in what git gained
swarmops git export "$GIT_SIDE" --ref main --branch review   # send the store's work to git
git -C "$GIT_SIDE" push origin review
```

Git then moves every commit between the git side and the remote, and a squash merge on the remote
writes a commit that the next import refuses with `E_GIT_IMPORT_DIVERGED`: move the trunk by a
fast-forward push of the reviewed commit instead (`git push origin review:stable`). The
[git round trip](../guides/git-round-trip.md) runs it command by command.
