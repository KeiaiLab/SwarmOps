# A store as a remote: clone, push and sync another store

After this page a store uses another store as its remote through `swarmops serve`: clone, fetch,
push and sync over ssh or HTTPS, and know what crosses between them.

**You need:** SwarmOps 0.12.0 or newer on both hosts; [GitHub as a remote](github-remote.md); a
store served [over ssh](../operate/serve-over-ssh.md) or [over HTTP](../operate/serve-over-http.md).

The commands are those of [GitHub as a remote](github-remote.md): `so clone`, `so fetch`,
`so push`, `so sync`. Only the remote differs: a store that `swarmops serve` serves over the git
wire. This page says what changes when the remote is a store.

```text
 host A                        host B                              host C
 ┌──────────────┐  so push,   ┌──────────────────────────┐  so push,  ┌──────────────┐
 │ store        │  so sync    │ swarmops serve           │  so sync   │ store        │
 │  + git mirror│ ──── git ─▶ │  access.toml: the key's  │ ◀─ git ─── │  + git mirror│
 │              │ ◀── git ─── │  identity, its grants    │ ─── git ─▶ │              │
 └──────────────┘  so fetch   │  the served store        │  so fetch  └──────────────┘
                              └──────────────────────────┘
```

*Each client store keeps a git mirror of the served store and moves packs with its own git; the
served store records what each key pushed.*

> ### In short
>
> - **Git commit ids are shared.** A commit on the served `main` has the same id and tree in every
>   store that fetched it; changeset ids are each store's own.
> - **The client lands.** `so sync` lands the store's changes onto the remote's head and pushes
>   them by compare-and-swap; the remote does not land a store's push.
> - **The key is the identity.** The served store knows the caller by its ssh key or web login,
>   never by the client's `--as`: every agent of one store is one identity there.
> - **The op log, agents, workspaces and land keys stay local.**

## Before you start

- **A served store.** The operator serves it as [Serve over ssh](../operate/serve-over-ssh.md) or
  [Serve over HTTP](../operate/serve-over-http.md) describe. Branches other than `main` and
  `release/<name>` need a `FORMAT 2` store on both sides.
- **A grant for the key.** The served store's `access.toml` names the key's identity: `read` for
  `clone` and `fetch`, `push` on `main` for `so push` and `so sync` of `main`, `push` on a branch
  pattern such as `feature/**` for that branch
  ([Write `access.toml`](../operate/serve-over-ssh.md#write-accesstoml)).
- **git 2.39 or later** on the client host, and the key or web login git uses for the URL
  ([Credentials](github-remote.md#credentials)).
- **An identity** for the commands that move `main` (`clone`, `sync`): `--as`, else
  `SWARMOPS_AGENT`. It names the agent in the client store only.

## Clone

```sh
export SWARMOPS_AGENT=operator/ada
so clone ssh://so-host/acme.git --into ~/.local/state/swarmops/repos/acme
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme
```

Over HTTPS the URL is the web server's prefix and the store's name,
`https://so-host/so/acme.git`. `clone` imports the served `main` into the new store's `main`,
commit by commit, and records the remote as `origin`. Each imported changeset keeps the served
commit's git id. `--branch feature/m` follows another served branch; a merge commit on it is
imported with both its parents.

## Fetch, push and sync

```sh
so fetch origin                  # the served store's new commits; no store ref moves
so sync status origin            # ahead and behind, offline
so push origin                   # main to the served main, forward only
so push origin feature/x         # a branch of its own (FORMAT 2), created or moved forward
so sync origin                   # land onto the served head, then push
```

They behave as against GitHub ([Fetch](github-remote.md#fetch), [Push](github-remote.md#push),
[Sync](github-remote.md#sync)). Against a served store:

- **The remote records the key.** Each push is a `ref_move` in the served store's op log, whose
  actor is the key's identity (`person/ada`); the changesets it brought are `commit` operations
  with the agent `unknown/unknown`. No operation of the served store names the client's agent.
- **`push` sends branches.** `main`, `release/<name>` and, in a `FORMAT 2` store, any branch. A tag,
  a remote-tracking ref or a target that starts with `refs/` is refused before git runs:
  `E_INVALID_ARG` with `details.reason` `source_kind` or `branch_target`. `main:refs/land/main`
  is one of them: over the git wire the remote does not land a store's push; `so sync` lands
  on the client.
- **Nothing is rewritten.** `rewritten` is 0 on a push of a history the store fetched whole.

## Several hosts at once

When two stores sync one served store at once, the second to push finds the remote moved: git's
lease refuses it, or the served store answers `stale: refs/heads/main moved; fetch first`. Either
reads as `remote_moved`, and `so sync` fetches, lands again onto the new head and pushes, up to
`--max-attempts` times (`attempts` counts them). Each change is then on the served `main` once.

A push held up after git read the served head can still lose to a push that moved `main` past
it: the served store then answers `non-fast-forward: main moves along first parents; push to
refs/land/main`, which reads as `E_REMOTE_REJECTED` `declined`, not retried. `main` moved in the
client store already (`details.main_moved`): run `so sync origin` again, and it lands what is left
onto the winner's head ([known limitations](../known-limitations.md#git-remotes)).

## Refusals

| The client sees | Why | What to do |
| --- | --- | --- |
| `E_REMOTE_DIVERGED`, `remote_moved`, exit 3 | The served branch moved since the last fetch | `so sync origin`, or fetch and push again |
| `E_REMOTE_DIVERGED`, `diverged`, exit 3 | The fetched histories differ | `so sync origin` |
| `E_CONFLICT`, exit 3 | A change does not merge onto the served head | Resolve as [When sync stops](github-remote.md#when-sync-stops) says |
| `E_REMOTE_REJECTED`, `declined`, exit 5, `remote_messages` holding `E_POLICY_DENIED: person/bob may not push main` | The key's grants do not cover the ref | Push to a branch the grant covers, or the operator widens it |
| `E_REMOTE_REJECTED`, `auth`, exit 5 | No grant names the key, or the store has no `access.toml` | The operator adds a grant |
| `E_REMOTE_REJECTED`, `not_found`, exit 5 | No store of that name is served | Check the name with the operator |
| `E_INVALID_ARG`, `source_kind` or `branch_target` | A tag or a `refs/` target | Push a branch |

The served store's own refusals are in
[Refusals a client sees](../operate/serve-over-ssh.md#refusals-a-client-sees); the store client
reads each by the code at the start of its line.

## When a transfer is cut

**A cut clone or fetch** is `E_REMOTE_UNREACHABLE` (exit 4, retryable) and changes no store ref. A
cut clone leaves the store it made, with the remote and no changeset of the cut pack
(`details.created`): run the same command again into the same directory, and it imports the whole
history. git starts the pack again from its start.

**A push whose answer was lost**, a killed client or a dropped connection after the served store
moved, leaves the remote holding the change. Run the push again:

```sh
so push origin
```

`up_to_date` with `old` different from `new`: the remote already held the commit, from a push
whose answer was lost; the tracking ref moved, nothing was sent. Run once more, `old` is `new`.

## What crosses, and what does not

| | Over the git wire (this page) | Native sync (planned) |
| --- | --- | --- |
| Who lands | `so sync`: the client lands onto the remote's head, then pushes by compare-and-swap; the remote records a `ref_move` by the caller. A push to `refs/land/<target>` is refused before git | The remote lands every change, keyed |
| Ids | A commit on the remote's `main` has one git id and one tree in every store that fetched it. A change made in a store keeps its changeset id there; other stores import its commit under ids of their own, and stores are compared by git commit id. A change that loses a race gets a new git commit id and a new changeset id; the old id is dropped locally and only the `SwarmOps-Changeset` trailer names it. No lineage is recorded | New ids upstream, the origin recorded |
| Op log | Does not cross. The remote logs `commit` (agent `unknown/unknown`) and `ref_move` by the ssh or web identity | Crosses with identity, grant and acting records |
| Agent | Kept as the git author (`authors.toml`); not an agent on the other side | Kept |
| Access | The remote's `access.toml` binds the key or web login. All agents of one store are that one identity on the remote; per-agent rules, grants and makers do not reach through a store | Per agent |
| Approvals | Do not cross | Carried |
| Land keys | In the trailer; `so sync` drops what the remote holds. The remote does not index them for its own `land --key`: a keyed land retried against the remote is not a replay there | Exactly once by key on the upstream |
| Workspaces, unlanded commits | Local. A branch push (`feature/x`) shares unlanded work | Local |
| Checks | Run by the client (`land-check`, `ws land --check`); the remote runs none on a served push | The upstream's gate |
| Resume | git's: a cut clone or fetch starts its pack again | No object fetched twice |
| Offline | Every operation but fetch, push and sync | The same |

Native sync is on the [roadmap](../roadmap.md#topology-3-local-swarmops-remote-swarmops).

## Limits to know

- **Tags do not cross.** A served store's tags stay in the client's mirror; `push` sends no tag.
- **No remote landing from a store.** `refs/land/<target>` takes a git client's push
  ([Serve over ssh](../operate/serve-over-ssh.md#what-a-client-types)), not a store's.
- **`git push --atomic`, push options and annotated tags are not served**
  ([Not served](../operate/serve-over-ssh.md#not-served)).
- More in the [known limitations](../known-limitations.md#git-remotes).

## Next

**Next:** [Serve over ssh](../operate/serve-over-ssh.md): serve a store to other stores and to
stock git.

- [GitHub as a remote](github-remote.md): the same commands against a git host.
