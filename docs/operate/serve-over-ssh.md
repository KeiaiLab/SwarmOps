# Serve over ssh

Let people and CI reach a store with nothing but stock git: `git clone`, `git fetch`, `git pull`
and `git push` over ssh, through `swarmops serve` as the forced command of an `authorized_keys`
line. Commands and outputs here follow the
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
- A filesystem that lets the serving account run files: git runs the hook `serve` writes in the
  store (`git/serve-hooks/proc-receive`), so a store mounted `noexec` refuses every push.

## How it works

```text
git (client) ──ssh──▶ sshd ──forced command──▶ swarmops serve --root /srv/so --as person/ada
                                                 │ 1 the caller: --as alone
                                                 │ 2 the store named by the client's path
                                                 │ 3 access.toml: may the caller read?
                                                 │ 4 git/serve.git refreshed from the store
                                                 ▼
                                    git upload-pack | git receive-pack on serve.git
                                                 │ a push: git checks the pack, then runs
                                                 ▼
                                    swarmops serve --hook proc-receive: per ref, access,
                                    import, compare-and-swap of the store ref, or a land
```

`serve` runs once per connection and ends with it; there is no daemon. The pack protocol is the
host git's, so every git client that talks to GitHub talks to it. What git advertises is the
export of each store ref's committed head, made again before every connection, so a client never
fetches what a crash could take back. `git/serve.git`, `git/SERVE`, `git/serve.lock` and
`git/serve-hooks/` in the store are a cache of that export: deleted, the next connection makes them
again, and a backup may leave them out.

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
`git gc --auto` packs later ([known limitations](../known-limitations.md#git-import-and-export)):
make it yourself before people clone a large store, with any read the account is granted.

## Write `access.toml`

The store grants nothing until its root holds an `access.toml`, read on every connection. Write it
as the serving account, beside `FORMAT` and `REPO`; no command writes it, and a change applies to
the next connection. A push to `refs/land/<target>` also reads it before each land attempt: a
`land` grant removed while a land retries after a lost race stops it before its next attempt,
with the refusal the connection would get and nothing written. An attempt already checked still
lands.

```toml
# Who may do what through `swarmops serve`. Read on every connection.
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

A push refused for one ref is `! [remote rejected] <ref> (<reason>)`; the push's other refs are
decided on their own, since pushes are not atomic. No text names a path of the server.

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

## Limits

- A push sends at most 2 GiB (`receive.maxInputSize`, [Push size](../reference/limits.md#push-size));
  git refuses a bigger pack before `serve` decides anything.
- A push names at most 1,000 refs ([Push commands](../reference/limits.md#push-commands)); past
  that, every ref is refused with `too_many_commands`.
- The serve lock waits 10 s, the store's writer lock wait; a refresh moves each served ref at most
  8 times against a concurrent writer before the connection is refused.

## Not served

Partial clone filters (the client warns and gets everything), `git push --atomic`, push options,
signed pushes, annotated tag objects, SHA-256 repositories, git LFS, submodules, server-side user
hooks, `git daemon`, a web interface, and `swarmops serve --stdio`, the store-to-store sync, which
is refused with `not_served`. A push to `main` moves it; it does not land: land through
`refs/land/main`.
