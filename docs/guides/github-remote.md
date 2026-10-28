# GitHub as a remote: clone, fetch, push and sync

A store can keep a git remote, GitHub or any git host, and move work to and from it itself:
`so clone` makes a store from a repository, `so fetch` takes in what the remote gained, `so push`
sends a store ref to a branch, and `so sync` lands the store's unpublished changes on top of the
remote's head and pushes them. Pull requests, CI and deployments stay on GitHub. This guide runs
the commands one by one, and says what to do when they stop.

**Pre-1.0** · Linux today · MIT licence

```text
 GitHub                         the store                      agents
 ┌──────────┐  so fetch     ┌───────────────┐
 │  main    │ ────────────▶ │ git mirror    │  import, no ref moved
 │  review  │ ◀──────────── │ (a cache)     │
 └──────────┘  so push      │               │  so sync: land on the
      ▲                     │  main ◀───────┼─ remote's head, move
      │ pull requests,      └───────────────┘  main, push       ◀── commit, land
      │ CI, deployments
```

*The store talks to the remote through a git mirror it keeps in its own directory. Your git, 2.39
or later, moves the packs; SwarmOps has no network code of its own.*

> ### In short
>
> - **Nothing is rewritten.** A commit fetched from GitHub keeps its id through any number of
>   round trips; a change made in the store exports to the same commit every time.
> - **Nothing is forced.** A push moves a branch only from the commit the last fetch saw, to a
>   descendant of it. A branch someone moved meanwhile fails the push, which sends nothing.
> - **Pull requests merge by any method.** After a merge commit, a squash or a rebase on GitHub,
>   `so sync` recognises the changes GitHub holds and drops them rather than land them twice.
> - **Your git, your credentials.** SwarmOps runs `git` with your credential helpers, ssh keys and
>   proxies, stores no credential, and never prompts.

## Before you start

- **SwarmOps 0.2.0 or later** ([Install](install.md)). The examples use `so`, the same program as
  `swarmops`.
- **git 2.39 or later** on `PATH`, or named by `SWARMOPS_GIT`. An older git fails the first remote
  command with `E_GIT_UNSUPPORTED` (`details.reason` `git_too_old`), and none with
  `git_missing`.
- **Access to the repository** as git has it: an ssh key, or a token for HTTPS
  ([Credentials](#credentials)).
- **An identity** for the commands that move `main` (`clone`, `sync`): `--as <family>/<instance>`,
  else `SWARMOPS_AGENT`. `fetch`, `push`, `remote` and `sync status` need none.

## Clone a repository

```sh
export SWARMOPS_AGENT=operator/ada
so clone git@github.com:acme/acme.git --into ~/.local/state/swarmops/repos/acme
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme
```

```json
{"ok":true,"op":"clone_repo","api":1,"result":{"repo":{"repo_id":"8e5730de…","name":"acme","main":"f3c47805…","store_version":2,"changesets":2,"refs":1},"remote":{"name":"origin","url":"git@github.com:acme/acme.git","branch":"main","lossy":[]},"fetch":{"remote":"origin","branch":"main","head":{"sha":"c6359690e26dc8f27294cffd27efcea1b5fb7192","changeset":"f3c47805…"},"imported":2,"branches":[{"branch":"main","new":"c6359690e26dc8f27294cffd27efcea1b5fb7192"}],"branches_truncated":false,"tags_fetched":0},"imported":2,"main":"f3c47805…"},"meta":{"repo":"…","elapsed_ms":223,…}}
```

The transcripts in this guide cut ids to eight digits and leave most of `meta` out; they come from
a run against a local repository served through a stand-in for ssh.

`clone` makes a new store in `--into`, records the remote as `origin` (`--remote` names it
otherwise), fetches it, and imports the branch it follows into `main`: the remote's default branch,
or `--branch`. Each imported changeset is the git commit it came from. The remote's other branches
are fetched into the store's mirror (`branches`) and not imported; its tags are fetched and never
imported. An empty remote gives a store with no `main`; the first `so push origin main` creates the
branch.

A clone that stops half-way (the network, a refusal) leaves the store it made, and says so
(`details.created`): run the same command again, with the same URL into the same directory, and it
continues.

### Which URLs

`https://` and `ssh://` URLs, and git's scp form `user@host:path`. A plain `http://` URL only to
a loopback address (for tests), and no `file://`, local path or other scheme: a remote is another
host. A URL with a password in it, or one that starts with `-`, is refused (`E_INVALID_ARG`): give
the token as below instead.

## Credentials

SwarmOps runs your git, so git's own ways to authenticate apply, as they do for `git fetch`:

- **ssh**: your keys, your agent (`SSH_AUTH_SOCK`), `~/.ssh/config` and `GIT_SSH_COMMAND`.
- **HTTPS with a credential helper**: whatever `git config credential.helper` names, such as the
  one `gh auth setup-git` sets up.
- **HTTPS with a token in an environment variable**: name the variable when you add the remote
  (`so clone … --token-env GH_TOKEN`, or `so remote add … --token-env GH_TOKEN`). The store records
  the variable's name, never its value; each command reads the variable and hands git the token as
  an HTTP header for that remote's URL alone (basic authentication, user name `x-access-token`),
  through the environment, never as an argument.

git never prompts: SwarmOps sets `GIT_TERMINAL_PROMPT=0`, so a missing credential fails at once
with `E_REMOTE_REJECTED` (`details.reason` `auth`, exit 5). A credential helper that opens a
window of its own ignores that setting and can wait for a person; under an MCP server nobody is
there to answer it. Give agents' hosts an ssh key or `--token-env` instead.

**Tokens in CI.** A GitHub Actions job passes its token in the variable it names:

```yaml
- env:
    GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
  run: |
    so clone https://github.com/acme/acme --into "$RUNNER_TEMP/acme" --token-env GH_TOKEN
```

The token needs to read the repository for `clone` and `fetch`, and to write its contents for
`push` and `sync`. git's output reaches an error's `details` only after the token, and its base64
form, are removed from it.

## Remotes

```sh
so remote list
so remote add upstream https://github.com/acme/acme.git --branch main --token-env GH_TOKEN
so remote remove upstream
```

```json
{"ok":true,"op":"remote_add","api":1,"result":{"name":"upstream","url":"https://github.com/acme/acme.git","branch":"main","token_env":"GH_TOKEN","lossy":[]},"meta":{"repo":"…","elapsed_ms":0,…}}
```

A remote's name is 1 to 63 lowercase letters, digits, dots, underscores and dashes, and not
`status`. Without `--branch`, `remote add` asks the remote for its default branch (a network call);
an empty remote gives `main`, with the warning `default_branch_unknown`. `remote remove` forgets
the remote and the mirror's refs of it, and removes no changeset. The remotes live in the store's
`remotes.toml`, which these commands write:

```toml
# Written by `so remote add|remove` and `so clone`; read by fetch, push, sync and remote list.
version = 1

[[remote]]
name = "origin"
url = "git@github.com:acme/acme.git"
branch = "main"

[[remote]]
name = "upstream"
url = "https://github.com/acme/acme.git"
branch = "main"
token_env = "GH_TOKEN"
```

`--lossy` takes the classes of content the import of the remote's commits may drop or normalize,
as [`git import`](git-round-trip.md#what-import-refuses) does; without it, such content fails
`fetch` with `E_GIT_IMPORT_LOSSY`.

## Fetch

```sh
so fetch origin
```

`fetch` brings the remote's branches and tags into the store's mirror, and imports the commits of
the followed branch into the store as changesets. It moves no store ref: `main` stays where it is.
Run again with nothing new, it imports nothing. Its response gives the followed branch's `head`
(the commit and its changeset), every branch it saw, and how many tags it took in.

## Sync status

```sh
so sync status origin --pretty
```

```text
{
  "ok": true,
  "op": "sync_status",
  "api": 1,
  "result": {
    "remote": "origin",
    "branch": "main",
    "fetched": true,
    "remote_head": {
      "sha": "c6359690e26dc8f27294cffd27efcea1b5fb7192",
      "changeset": "f3c47805…"
    },
    "base": "f3c47805…",
    "ahead": 1,
    "behind": 0,
    "unpublished": [
      {
        "id": "b9a88bdb…",
        "parents": [
          "f3c47805…"
        ],
        "author_agent": "claude/worker-1",
        "created_at": 1792671566000,
        "title": "return two"
      }
    ],
    "truncated": false
  },
  …
}
```

`sync status` compares `main` with the remote branch as the last fetch saw it: `ahead` counts the
store's changes on `main` that the remote does not hold (`unpublished` lists them, newest first),
`behind` the remote's commits that `main` lacks. It is offline: it takes no lock and runs no git,
so run `so fetch` first to see the remote as it is now.

## Push

```sh
so push origin main:review
```

```json
{"ok":true,"op":"push","api":1,"result":{"remote":"origin","branch":"review","status":"pushed","old":"c6359690e26dc8f27294cffd27efcea1b5fb7192","new":"1cfcde10ca51882cdc819207aef75ca87599ae89","head_changeset":"b9a88bdb…","objects_written":4,"rewritten":0},"meta":{"repo":"…","elapsed_ms":40,…}}
```

`push <remote> [<ref>[:<branch>]]` sends a store ref (`main` by default, or `release/<name>`) to a
remote branch (by default the followed branch for `main`, the same name for `release/<name>`). It
exports the ref into the mirror on top of what the last fetch saw of the branch, so the export only
appends, then has git push it with a lease on that commit: the remote moves only from there, to a
descendant of it. `status` is `created` for a new branch, `pushed` when it moved, and `up_to_date`
when there was nothing to send, in which case no git runs at all. `rewritten` counts commits the
export had to write anew where the remote's history was imported with a loss
([known limitations](../known-limitations.md#git-remotes)).

A push never forces. When the branch moved since the last fetch, it sends nothing:

```json
{"ok":false,"op":"push","api":1,"error":{"code":"E_REMOTE_DIVERGED","message":"main on \"origin\" moved since the last fetch: nothing was sent","retryable":false,"exit":3,"details":{"branch":"main","reason":"remote_moved","remote":"origin"},"hint":"swarmops fetch origin"},…}
```

Fetch and push again when the store's work already sits on the new head; otherwise sync. A branch
the store is behind (`behind`) or has diverged from (`diverged`) fails before git runs, with the
same code. A refusal by GitHub (a branch rule, a ruleset, a hook) is `E_REMOTE_REJECTED`
(`details.reason` `declined`, with GitHub's messages in `remote_messages`).

**GitHub's limits.** When the remote's host is `github.com`, a push checks GitHub's documented
limits before it sends anything: a file over 100 MiB fails with `E_TOO_LARGE`
(`details.scope` `remote_file`, with its path), and so does a push over 2 GB (`remote_push`);
each file over 50 MiB adds a warning (`github_large_file`), as GitHub warns. Other hosts apply
their own limits at the server.

## Sync

```sh
so sync origin
```

```json
{"ok":true,"op":"sync","api":1,"result":{"remote":"origin","branch":"main","status":"pushed","attempts":1,"published":[{"changeset":"cfc2ab1b…","landed_as":"68bea641…","sha":"261af3df9e43e917507463391186681f56e931a4"}],"dropped":[],"main":{"old":"cfc2ab1b…","new":"68bea641…"},"remote_head":{"old":"9b50557ece8d72c6e96a6668c0f273da985e9f53","new":"261af3df9e43e917507463391186681f56e931a4"}},"meta":{…}}
```

Here a person had pushed a commit to `main` on the remote after the store's last fetch, so the
`push` above failed. `sync` does what a person would do by hand, in one command:

1. It fetches the remote.
2. It finds the store's unpublished changes: those on `main` above the newest changeset the
   remote's head reaches.
3. It drops those the remote holds already ([below](#what-sync-recognises)).
4. It lands each of the others onto the remote's head, oldest first, with the three-way merge
   `land` uses, without moving any ref. Each keeps its message, its author and its land key, so an
   agent that retries a keyed land afterwards gets a replay, not a second land.
5. It moves `main` to the result, by compare-and-swap.
6. It pushes `main` to the remote branch, as `push` does.

When the remote or `main` moved meanwhile, it starts again from the fetch, up to
`--max-attempts` (8 by default) times. `status` says what it did: `up_to_date`, `pushed`,
`fast_forwarded` (every change was on the remote already: `main` moved to the remote's head), or,
with `--no-push`, `landed_locally`.

`main` moves before the push, so a push that fails after it (refused by a rule, the network) leaves
`main` landed on the remote's head; the error says so (`details.main_moved`), and the next sync
pushes what is left. `swarmops op undo` of the sync's operation takes `main` back.

### Branches that take pull requests only

`so sync origin --no-push` does everything but the push: use it for a branch that only pull
requests may change. Send the work to a branch of its own, open the pull request with GitHub's
tools (`gh pr create`; SwarmOps calls no GitHub API), and after GitHub merged it, sync:

```sh
so push origin main:review
gh pr create --base main --head review --fill
# … review, CI, merge on GitHub by any method …
so sync origin --no-push
```

```text
{
  "ok": true,
  "op": "sync",
  "api": 1,
  "result": {
    "remote": "origin",
    "branch": "main",
    "status": "fast_forwarded",
    "attempts": 1,
    "published": [],
    "dropped": [
      {
        "changeset": "b9a88bdb…",
        "by": "trailer"
      }
    ],
    "main": {
      "old": "b9a88bdb…",
      "new": "b6371f91…"
    },
    "remote_head": {
      "old": "581c0b5a7daa6ae643081c2ff92fb7204615c6e6",
      "new": "581c0b5a7daa6ae643081c2ff92fb7204615c6e6"
    }
  },
  …
}
```

### What sync recognises

A commit the store exports ends with three lines that name what it came from:

```text
SwarmOps-Changeset: b9a88bdbca90d75ffeaf8410dcf05ceabb87c5278cfefaef5e48d5496d0f5ec2
SwarmOps-Land-Key: 0192d5a0-0000-7000-8000-000000000001
SwarmOps-Squashed-From: 56f3ac618e9e065b173594d4f92c6309da49a1933d4d7e0418a3ef0a4edf1fd7
```

A change is on the remote when one of the remote's new commits names it on such a line, indented
or after a list bullet (`* `, `- `) included, as GitHub's default squash message lists the
squashed commits' messages (`by: "trailer"`); or when, of the changes left, the oldest ones merge
onto the remote's head as no change at all (`by: "no_op"`): a squash whose message lost the lines,
or a change someone made again by hand. A merge commit needs neither, since the remote then holds
the exported commits themselves. A dropped change stays in the store's history, and
`swarmops op undo` of the sync's move brings it back to `main`.

A line is taken as it stands: anyone who can push to the remote can name a change in a commit
message, and sync then drops that change without checking the commit's content.

## Commit authors

A change made in the store exports with its agent as author and committer, at an address of the
form `<instance>@<family>.swarmops.local`, which GitHub links to no account. A branch rule that
requires known authors refuses such commits. The store's `authors.toml`, written by a person,
gives an agent family the address (and the name) its new changes are signed with:

```toml
version = 1

[[author]]
family = "claude"
email = "12345678+acme-agents@users.noreply.github.com"
name = "acme-agents"
```

```text
$ git log --format='%h %an <%ae>' -2 origin/main
52aafd1 acme-agents <12345678+acme-agents@users.noreply.github.com>
261af3d claude/worker-1 <worker-1@claude.swarmops.local>
```

The file is read each time a changeset is signed: by `commit`, `land` and `sync`. Changes made
before it keep their signatures, so their commits export as they did. Without `name`, the agent's
name stays. A value holding `<`, `>`, a line break or a NUL, an empty one, two entries for one family, or
a field the file does not know fails the signing with `E_STORE_CORRUPT`. SwarmOps signs no commit:
a branch that requires signed commits refuses those it makes.

## When sync stops

**On a conflict.** A change that does not merge onto the remote's head stops the sync with
`E_CONFLICT` (exit 3): nothing moved, nothing was sent. The error names the change and the head it
met (`details.change`, `details.onto`) and carries the report `land` gives:

```json
{"ok":false,"op":"sync","api":1,"error":{"code":"E_CONFLICT","message":"5cbbb8c0… conflicts with 028564b4…","retryable":false,"exit":3,"details":{"change":"5cbbb8c0…","onto":"028564b4…","report":{…}},"hint":"swarmops cat 028564b4…:src/lib.rs, then swarmops land 5cbbb8c0… --resolve <file> (resolutions over MCP) with each path's resolution and against = 028564b4…"},…}
```

Put `main` on the remote's head, land `main`'s old head with the resolutions, and sync:

```sh
OLD=$(so resolve main | field result.id)       # field: the daily loop's helper
so ref set main 028564b4c7213a2f1a6ac788aa25dbc0bd4726f765dbd0587726cce6d0aa336a --expect "$OLD"
cat resolve.json
so land "$OLD" --resolve resolve.json --key "$(uuidgen)"
so sync origin
```

```text
{"against": "028564b4c7213a2f1a6ac788aa25dbc0bd4726f765dbd0587726cce6d0aa336a",
 "files": {"src/lib.rs": {"content": {"inline": "pub fn f() -> u32 { 45 }\n"}}}}
```

The land takes every unpublished change at once, as one changeset, and the sync then pushes it.
The [Commit and land](commit-and-land.md) guide explains resolutions.

**On a change it does not recognise.** A pull request squashed with a message that lost the
`SwarmOps-` lines, whose lines were then edited again on GitHub before the sync, matches neither
rule: the sync stops with `E_CONFLICT` naming that change. When `so sync status origin` and the
remote's history show the change is there, move `main` to the remote head's changeset (the
`remote_head.changeset` of `sync status`) with `so ref set main <that id> --expect <main>`, and
sync again.

**On a merge.** An unpublished merge changeset on `main` (from a `git import` into `main`) stops
the sync with land's merge error: push such history with git.

**On the network.** `E_REMOTE_UNREACHABLE` (exit 4, retryable) changes nothing; every local command
keeps working, and lands made meanwhile wait as unpublished.

**On another sync.** A second `fetch`, `push`, `sync` or `remote` command on the same store while
one runs fails at once with `E_BUSY` (`details.lock` `git_mirror`, retryable). Agents that only
commit and land are never blocked by a sync: the store's own lock is not held across the network.

## Through MCP

`swarmops mcp` serves five of these commands as tools: `remote_list`, `fetch`, `push`, `sync` and
`sync_status`. Each takes a remote by its name only. `clone` and `remote add|remove` are CLI only,
so that an agent cannot point the store at a URL of its choosing. A tool call waits for git, so a
long fetch holds that agent's session until it ends. The MCP server's environment is the
credentials' environment: start it with the ssh agent or the token variable the remotes need.

## Limits to know

- **One branch is followed.** `main` follows one remote branch; the others are fetched into the
  mirror and imported only when a push or sync needs them. Tags are never imported.
- **The mirror doubles the space** a repository takes on disk: it is a git repository of the
  remote's history beside the store's own.
- **Performance targets are missed** on long histories: on 12,000 commits a one-commit push costs
  3.9× what it does on 1,000, and a sync of one land takes 8.8 s against a target of 1 s
  ([perf](../perf.md#git-remotes-m13-the-three-topology-2-targets)).
- More in the [known limitations](../known-limitations.md#git-remotes).
