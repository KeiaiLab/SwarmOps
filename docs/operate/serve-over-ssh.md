# Serve over ssh

After this page people and CI reach a store with stock git alone: clone, fetch, pull and push over
ssh, through `swarmops serve` as the forced command of an `authorized_keys` line.

**You need:** SwarmOps 0.7.0 or newer on Linux (`swarmops --version`); [Stores](stores.md); git
2.39 or newer on the server (`git --version`); an OpenSSH server (`ssh -V`).

Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages); the client
outputs come from a run with git 2.43.0. Over HTTPS instead: [Serve over HTTP](serve-over-http.md),
which reuses this page's access file and refusals.

## Before you start

- `swarmops` and git 2.39 or later on the server ([Install and upgrade](install-and-upgrade.md)),
  an OpenSSH server, and any git on the clients.
- A store under one directory, here `/srv/so`, owned by the account that serves it
  ([Stores](stores.md#set-the-permissions)). A store's name is its directory's name: one segment of
  `[a-z0-9][a-z0-9._-]{0,62}`, here `acme`.
- A `FORMAT 2` store for branches beyond `main` and `release/<name>`, and for tags
  (`swarmops init --format 2`, or [`swarmops upgrade`](../reference/commands/upgrade.md)).
- A filesystem that lets the serving account run files: git runs the hooks `serve` writes in the
  store (`git/serve-hooks/pre-receive` and `git/serve-hooks/proc-receive`), so a store mounted
  `noexec` refuses every push.

## How it works

```text
git (client) ──ssh──▶ sshd ──forced command──▶ swarmops serve --root /srv/so --as person/ada
                                                 │ 1 the caller: --as alone
                                                 │ 2 the store named by the client's path
                                                 │ 3 access.toml: may the caller read?
                                                 │ 4 git/serve.git refreshed from the store,
                                                 │   unless git/serve.state shows nothing moved
                                                 ▼
                                    git upload-pack | git receive-pack on serve.git
                                                 │ a fetch: swarmops serve --hook
                                                 │ pack-objects sends the pack, from
                                                 │ git/serve-cache/ when an identical
                                                 │ request was answered before
                                                 │ a push: git checks the pack in its
                                                 │ quarantine, then runs
                                                 ▼
                                    swarmops serve --hook pre-receive: per ref, access,
                                    forward or force, import; one refused ref refuses the
                                    push, and git drops the quarantine
                                                 ▼
                                    swarmops serve --hook proc-receive: per ref, the same,
                                    then compare-and-swap of the store ref, or a land
```

`serve` runs once per connection and ends with it; there is no daemon. The pack protocol is the
host git's, so every git client that talks to GitHub talks to it. What git advertises is the
export of each store ref's committed head, made again before every connection, so a client never
fetches what a crash could take back. `git/serve.state` names the store ref, the changeset and
the commit of each export; a connection whose store refs and `serve.git` refs both match it
advertises `serve.git` as it is, taking neither the store's writer lock nor `git/serve.lock`, so a
fetch with nothing new does not wait behind the agents' lands. Anything else (a ref moved, the file
missing or damaged, a `serve.git` ref moved by hand) refreshes first, as before.

Refreshes coalesce. `git/serve.state` (version 2) also says `covered`: how far into the store's
journal the last full refresh read. A connection that started before that point finds `serve.git`
current even when the store has moved since, because that refresh already holds every land
acknowledged before the connection started; a refresh that finds such a newer export once it has
the lock writes nothing. A refresh lets `git/serve.lock` go while it waits for the store's writer
lock, and a preparation lets it go between its rounds, so connections and pushes do not wait
behind the agents' lands. A state of version 1 (an older `swarmops`) is read as not current: the
next connection refreshes and writes version 2.

git hands each fetch's pack to `serve --hook pack-objects` (its `uploadpack.packObjectsHook`,
which `serve` sets in git's environment). It sends only commits a served ref is or reaches: a
want of anything else, such as the commit of a refused push that git kept in `serve.git`, is
refused before any byte, with either protocol. Identical requests (the same wants, haves, depth
and options, the same git) share one pack in `git/serve-cache/`: the first produces it, and
the others are sent it, after its header's BLAKE3 digest is checked against its bytes; an entry
that fails the check is removed and the pack produced again. The cache keeps at most 4 GiB a
store, the least recently used removed first.

`git/serve.git`, `git/SERVE`, `git/serve.lock`, `git/serve.state`, `git/serve-hooks/` and
`git/serve-cache/` in the store are a cache of that export: deleted, the next connection makes
them again, and a backup may leave them out. `git/serve-records.jsonl`, its older file and
`git/serve-split/` are [records](#records) of the requests served, which a backup may leave out
too.

## The serving account

One Unix account runs `serve` for every key: the account that owns the stores, since their files
are readable by their owner alone. Its environment does not name the caller: `serve` ignores
`SWARMOPS_AGENT` and `SWARMOPS_RUN_ID`, so a key line without `--as` is refused rather than given
the account's identity.

**Its state directory.** The account keeps one memo key per store,
`$XDG_STATE_HOME/swarmops/memo-keys/<repo_id>.key` (`~/.local/state` without `XDG_STATE_HOME`;
`SWARMOPS_MEMO_KEY_DIR` moves the directory), made at its first connection. With it, the refresh
before each connection stops its walk of the history at what it exported before; without a home
directory it stays correct but walks the history every time. Give the account a home directory it
can write ([Backup and restore](backup-and-restore.md) says what the key is).

**Its first connection** exports the store's whole history into `serve.git`, as loose objects that
`git gc --auto` packs later ([known limitations](../known-limitations.md#git-import-and-export)).
Prepare a large store before people clone it, as the serving account:

```sh
swarmops --repo /srv/so/acme serve --prepare --seed-from /srv/import/acme.git
```

```text
{"ok":true,"op":"serve_prepare","api":1,"result":{"refreshed":{"moved":1,"removed":0,"objects":0},"rounds":1,"seeded":1,"skipped":null,"warnings":[]},...}
```

`--seed-from` names the repository `git import` read: `serve --prepare` fetches the imported
commits' objects from it into `serve.git`, exports (finding every commit there, so writing none),
and packs the whole history with a bitmap. Without `--seed-from` it exports what `serve.git`
lacks. It needs no `--root`; it waits for `git/serve.lock` as a connection does (`--prepare-wait
zero` gives up at once and marks the lock's holder to run again), and exits 0 when it finds
nothing to do. It helps only when it runs as the account that serves the store and that the agents
run as: it writes `serve.git` and uses that account's memo key. Without the memo key, on a store
whose `main` head is no imported commit, it exports nothing and says so in `warnings`; the next
connection then refreshes as before.

**After every ref move** of a store that holds `access.toml` (`land`, `commit --advance`, `ref
set`, `ref delete`, `op undo`, `sync`, `fetch`, `git import` and `ws land`, from the command line
or as an MCP tool) `swarmops` starts `swarmops serve --prepare --prepare-wait zero` for that store
once its response is out (after `git import`, with `--seed-from` the repository it read). That
run is detached: its own process group, no stdin, stdout or stderr, and nobody waits for it; the
MCP server reaps the ones it started. When another refresh holds `git/serve.lock` it leaves
`git/serve.dirty` for that one and exits, so a burst of lands costs one preparation running at a
time. A preparation that fails changes nothing the operation returned; the next connection
refreshes as before. It helps only when the agents run as the same account as `serve`.

## Write `access.toml`

The store grants nothing until its root holds an `access.toml`, read on every connection. Write it
as the serving account, beside `FORMAT` and `REPO`; no command writes it, and a change applies to
the next connection. A push to `refs/land/<target>` also reads it before each land attempt: a
`land` grant removed while a land retries after a lost race stops it before its next attempt,
with the refusal the connection would get and nothing written. An attempt already checked is not
stopped by the check: it can still land, lose its race or conflict.

```toml
# Who may do what through `swarmops serve`.
# Read on every connection and before each land attempt.
version = 1

[[grant]]
who = ["person/ada", "ci/*"]          # an identity, or <family>/* for every instance of a family
read = true                            # clone, fetch, pull
push = ["release/*", "feature/**", "tags/**"]   # refs a push may create and fast-forward
force = ["feature/**"]                 # of those, refs a push may also move backwards or delete
land = ["main", "release/*"]           # targets a push to refs/land/<target> may land onto

[[grant]]
who = ["person/bob"]
read = true
```

- **Names** are store ref names: `main`, `release/1.4`, `feature/x`, `tags/v1.4.0`. A pattern is
  an exact name, `*` for one segment without `/`, or a trailing `**` for every depth below.
- **Grants add up**, and there are no deny rules: what any grant naming the caller allows, the
  caller may do, whatever their order.
- `push`, `force` and `land` each imply `read`. `force` acts only on refs `push` also covers, and
  a deletion needs `force`.
- `main` never moves backwards and is never deleted: a `force` that covers `main` makes the file
  invalid. The example lets people change `main` only by a land; a grant with `push = ["main"]`
  lets a push move it forward along first parents as well.
- The file holds at most 1 MiB ([Access file](../reference/limits.md#access-file)).

| The file | Every caller gets |
| --- | --- |
| Missing | [`E_AUTH`](../reference/errors.md#e_auth), `details.reason` `no_access_file` |
| Not TOML, an unknown key, `version` other than 1, a bad pattern, a `force` covering `main`, more than 1 MiB | [`E_POLICY_DENIED`](../reference/errors.md#e_policy_denied), `details.reason` `access_file_invalid`, with the line |

```text
fatal: remote error: E_POLICY_DENIED: access.toml has force "**", which covers main (line 22)
```

## Add a key line

In `~/.ssh/authorized_keys` of the serving account, one line per person or tool, each naming its
own identity with `--as` ([Identities](stores.md#identities)):

```text
command="swarmops serve --root /srv/so --as person/ada",restrict ssh-ed25519 AAAA… ada@laptop
command="swarmops serve --root /srv/so --as ci/runner-1",restrict ssh-ed25519 AAAA… ci
```

`restrict` turns off forwarding, the pty and `~/.ssh/rc`; the forced command runs whatever the
client asks, and `serve` reads the request from `SSH_ORIGINAL_COMMAND` without a shell. It runs
`git-upload-pack` and `git-receive-pack` (or `git upload-pack`, `git receive-pack`) on a path that
names one store under `--root`, and refuses anything else. sshd starts the command through the
account's login shell, so `swarmops` must be on that shell's `PATH`, or named by its full path.

## Let protocol v2 through

git sends the protocol it wants in `GIT_PROTOCOL`, which sshd drops unless it accepts it. In
`sshd_config`, then reload sshd:

```text
AcceptEnv GIT_PROTOCOL
```

Without it, clients fall back to protocol v0, which `serve` also serves; v2 sends only the refs a
fetch asks for.

## What a client types

Nothing but stock git, with the store's name as the repository (`.git` is optional):

```sh
git clone ssh://so-host/acme.git              # main, the branches and the tags
git pull --rebase                             # fetch, then rebase onto main
git push origin HEAD:refs/land/main           # land: squashed onto main, as an agent lands
git push origin feature/x                     # a branch of its own (FORMAT 2)
git push --force origin feature/x             # with force granted
git push origin v1.4.0                        # a lightweight tag (FORMAT 2)
git push origin main                          # with push granted on main: forward only
```

A clone's commits keep their ids: the store's history imported from git exports back as itself.

**A land** squashes the pushed commits into one changeset on `main`, as `swarmops land` does, and
names the commit it made; nothing is left under `refs/land/`:

```text
remote: landed onto main as 211c210a39dba59a1f0897b5205bd65678ef79b8: git fetch brings it in
To ssh://so-host/acme.git
 * [new reference]   HEAD -> refs/land/main
```

The landed commit is new to the client, whose author is the pushed commit's and whose committer
is the caller; `git pull --rebase` drops the local commit it replaces:

```text
warning: skipped previously applied commit e6abb43
…
Successfully rebased and updated refs/heads/main.
```

The same push again (a dropped connection, a CI retry) carries the same land key, finds the land
on `main` and writes nothing (`remote: landed already onto main as 211c210…`). The same commit
pushed by another identity is a land of its own: a change of its own, or a refusal with
`E_NOTHING_TO_LAND` when `main` holds its changes already.

**A conflict** writes nothing; the report comes on git's `remote:` lines, at most 50 paths
([Land conflict lines](../reference/limits.md#land-conflict-lines)):

```text
remote: E_CONFLICT: 1 path conflicts with main at b62a907
remote:   README.md: both changed lines 1; main's change by unknown/unknown
remote: resolve it on top of main, then push again
To ssh://so-host/acme.git
 ! [remote rejected] HEAD -> refs/land/main (conflict)
```

`unknown/unknown` names a change that came in from git, which carries no agent; a change an agent
committed in the store names that agent. Fetch, rebase onto `origin/main`, resolve, push again.

## Refusals a client sees

A refusal before git starts is one line, `fatal: remote error: <CODE>: <message>`, and the
envelope after it, which ssh passes on from `serve`'s stderr:

```text
fatal: remote error: E_AUTH: the store has no access.toml: serve grants nothing without one
{"ok":false,"op":"serve","api":1,"error":{"code":"E_AUTH","message":"the store has no access.toml: serve grants nothing without one","retryable":false,"exit":5,"details":{"reason":"no_access_file"},"hint":"the operator writes access.toml in the store's root: version = 1, then one [[grant]] per set of callers (who, read, push, force, land)"},"meta":{"elapsed_ms":1,"resolved":{},"auth":"trust"}}
```

A push with one refused ref is refused whole, before any of its objects enters `serve.git`: every
ref is `! [remote rejected] <ref> (pre-receive hook declined)`, and a `remote:` line per ref gives
the reason, `<ref> <CODE>: <message>` (a ref refused only with the others reads `accepted alone`).
Push refs that must land independently in separate pushes. A ref that loses a race after that
check, to another push or a land conflict, is refused alone: `! [remote rejected] <ref>
(<reason>)`. No text names a path of the server.

| The client sees | Why | What to do |
| --- | --- | --- |
| `E_AUTH: serve names no caller: --as is not given, …` | The key line has no `--as` | Operator: add `--as` to the line |
| `E_AUTH: no grant of access.toml names person/eve` | No grant names the caller | Operator: add a grant, or the caller uses its own key |
| `E_AUTH: the store has no access.toml: …` | [Write `access.toml`](#write-accesstoml) | Operator |
| `E_POLICY_DENIED: access.toml … (line N)` | The file is invalid | Operator: fix line N |
| `E_POLICY_DENIED: person/bob may not push feature/b` | The caller's grants do not cover the action and ref | Push to a ref it covers, or land; or the operator widens the grant |
| `E_INVALID_ARG: the path names no store` | The URL's path is not one store name | `ssh://<host>/<name>.git` |
| `E_NOT_A_REPO: no store named nope is served` | No such store under `--root` | Check the name with the operator |
| `E_INVALID_ARG: serve runs git-upload-pack and git-receive-pack alone` | `ssh so-host <command>`, or `git archive --remote` | Clone, fetch, pull or push with git |
| `E_BUSY: …` | Another connection's refresh held `git/serve.lock` for 10 s | Connect again |
| `remote: {"ok":false,…"E_POLICY_DENIED",…"not_served"…}`, then git's `bad pack header` | A fetch by id of an object no served ref reaches | Fetch a branch or a tag |
| `non-fast-forward: main moves along first parents; push to refs/land/main` | A push to `main` that is not forward along first parents, such as the merge `git pull --no-rebase` makes | `git pull --rebase`, or push to `refs/land/main` |
| `! [rejected] … (non-fast-forward)` | A branch moved backwards without `--force` | `--force`, if `force` covers the branch |
| `stale: refs/heads/feature/y moved; fetch first` | Another push moved the ref after this client's fetch | Fetch, then push again |
| `E_REF_PROTECTED: ref "main" is protected: it is never deleted` | A deletion of `main` | None: `main` stays |
| `E_TAG_IMMUTABLE: tag "tags/v0.1.0" cannot move: …` | A tag pushed again at another commit | Delete it (with `force`), then push it again |
| `E_INVALID_ARG: … is an annotated tag, which the store does not hold: …` | `git tag -a` | Push a lightweight tag |
| `E_REF_NAME_INVALID: refs/notes/x is not served: …` | A ref outside `refs/heads/`, `refs/tags/` and `refs/land/` | Push a branch or a tag |
| `E_REF_NAME_INVALID: … names a ref the store reserves` | A branch under `remote/` | Another name |
| `E_FORMAT_REQUIRED: refs/heads/feature/z needs a store of format 2: …` | A branch or tag in a `FORMAT 1` store | Operator: `swarmops upgrade` |
| `E_GIT_IMPORT_LOSSY: …` | The pushed history holds what the store cannot keep, such as a submodule | Push a history without it |

## Records

`serve` appends one JSON line to the store's `git/serve-records.jsonl` for each request it served
from the store on: a clone, a fetch, a push, or a refusal after the store was found. Each line is a
`ServeRecord` (`swarmops schema ServeRecord`):

```sh
tail -n 1 /srv/so/acme/git/serve-records.jsonl
```

```text
{"at":1794048398609,"identity":"person/ada","service":"clone","prepare_ms":50,"send_ms":6335,"bytes":6293595,"exit":0}
```

`at` is when the connection started (Unix milliseconds) and `identity` the caller `serve`
resolved. `service` is `clone` for a fetch that excludes nothing (no have, no excluded commit, no
shallow client), else `fetch` (a fetch with nothing new too), or `push`. For a fetch,
`prepare_ms` runs to the pack's first byte (the refresh, the negotiation, the pack produced or
found in the cache) and `send_ms` from there to the end, so a slow client shows as `send_ms`; for
a push, `send_ms` runs to the start of the push's check (receiving it and git's own checks) and
`prepare_ms` from there to the end (judging, importing, landing, exporting). A request that sent
no pack, or a push that never reached its check, is all `prepare_ms`. `bytes` counts the pack sent
or received, `exit` is git's (`128 + signal` for a git a signal ended) or the refusal's, and
`code` names a refusal's error code, a push's first refused ref among them.

The hooks pass their split to the parent `serve` in `git/serve-split/<connection>`, which it
removes after reading. Lines are not synced: the records are diagnostics, a reader skips a torn
last line, and nothing reads them for correctness. A record that cannot be written is a `warning:`
line on stderr; the request stands. Records hold no path and nothing of the serving account's
environment. Past 64 MiB the file is renamed `serve-records.1.jsonl`, replacing the one older
file.

## Limits

- A push sends at most 2 GiB (`receive.maxInputSize`, [Push size](../reference/limits.md#push-size));
  git refuses a bigger pack before `serve` decides anything.
- A push names at most 1,000 refs ([Push commands](../reference/limits.md#push-commands)); past
  that, every ref is refused with `too_many_commands`.
- The serve lock waits 10 s, the store's writer lock wait; a refresh moves each served ref at most
  8 times against a concurrent writer before the connection is refused.
- The pack cache holds at most 4 GiB a store
  ([Serve pack cache](../reference/limits.md#internal-values)); a larger pack is sent and not
  kept. A fetch waits at most 10 s for another connection producing the same pack, then
  produces its own.
- The records of a store take at most 64 MiB before they move to the one older file
  ([Serve records](../reference/limits.md#internal-values)).

## Not served

Partial clone filters (the client warns and gets everything), `git push --atomic`, push options,
signed pushes, annotated tag objects, SHA-256 repositories, git LFS, submodules, server-side user
hooks, `git daemon`, a web interface, and `swarmops serve --stdio`, the store-to-store sync, which
is refused with `not_served`. A push to `main` moves it; it does not land: land through
`refs/land/main`.

## Next

**Next:** [Serve over HTTP](serve-over-http.md): the same store over HTTPS, with the same access
file.

- [Where serve answers differently from git](serve-differences.md): each difference a client may
  see.
