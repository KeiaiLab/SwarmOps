# Making room

Remove what a store may lose: its leftovers with `swarmops gc`, and its derived values by hand,
with every writer stopped.
Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

## What may go

A store only grows ([Limits](limits.md)) but for two kinds of file. This is what the storage spec
gives for each, and no more:

- **Leftovers**, which `swarmops gc` removes (the M6 design, section 8): files in `objects/tmp/`
  and `meta/derived/tmp/` older than a day (`object_temp`, `derived_temp`), where a write that died
  on the way left the file it was staging; a crashed `init`'s `meta.tmp/` (`meta_staging`); the
  values of a built-in derivation's older versions (`derived_version`); and the old files an
  upgrade left (`upgrade_residue`). No frame names an object before it is linked and durable (§2,
  rules 1 and 2), so a staging file is never the only name of an object that a changeset needs,
  and a day keeps the files of writes still in flight.
- **The derived values**, `meta/derived/`, which may go whole. They are a cache that neither the
  journal nor any other file names (§3), and removing a value's file, or `meta/derived/`, makes the
  value derived again (§5).

`gc` deletes no object and no changeset, and nothing a ref, a workspace or the git mirror reaches:
the objects, the journal, the index, the refs, `quarantine/` and `git/` stay as they are. It counts
the orphan objects (`orphan_objects`), those no changeset reaches, and keeps them.

## 1. Stop every writer

Removing the derived values needs the store to itself. A writer is every process that can write it: the agents'
sessions and their MCP servers (a server lives as long as its session), the loop's commands and
`swarmops derive`, and also a read, which stores the values it derives. `flock` on `meta/lock` stops
none of them, since objects and derived values are written without it. Check that no `swarmops` is
left (`so` is the same program):

```sh
pgrep -ax 'swarmops|so'
```

```text
1503 swarmops mcp --repo …
```

That is an MCP server that a client started for its session: end the session. With nothing left, the
command prints nothing and exits 1.

## 2. Remove the leftovers of interrupted writes

`gc` needs no writer stopped: it takes the store's maintenance lease and its lock, and leaves the
temp files younger than a day. `--dry-run` reports what it would remove and removes nothing:

```sh
swarmops gc --dry-run
swarmops gc
ls -a "${SWARMOPS_REPO:?}/objects/tmp"
```

```text
{"ok":true,"op":"gc","api":1,"result":{"removed":[{"kind":"object_temp","count":1}],"bytes":1000,"orphan_objects":0},…}
{"ok":true,"op":"gc","api":1,"result":{"removed":[{"kind":"object_temp","count":1}],"bytes":1000,"orphan_objects":0},…}
.
..
.tmpoyk2fg
```

This store had two leftovers of writers killed in the middle of a write; one was set back two days
with `touch -d`, and the other, as young as it was, stays. A second `gc`, `repair` or `upgrade`
started meanwhile fails at once with the retryable `E_MAINTENANCE_BUSY`.

## 3. Remove the derived values

The derived values come off whole, and `verify --deep` has no stored value left to check:

```sh
du -sh meta/derived; find meta/derived -type f | wc -l
rm -rf meta/derived
swarmops verify --deep
```

```text
344K	meta/derived
38
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":45,"changesets_checked":15,"problems":[],"findings":[],"truncated":false,"found":0},"meta":{"repo":"…","elapsed_ms":2,…}}
```

## 4. Derive them again

A read derives the values it needs and stores them in a `meta/derived/` that it makes again,
`swarmops derive` makes the eager ones for every changeset at once, and `verify --deep` then checks
the stored values of the built-in derivations against the changesets (§5):

```sh
swarmops history main:src/lib.rs --limit 1 --human
find meta/derived -type f | wc -l
swarmops derive
swarmops verify --deep
```

```text
path: src/lib.rs
entries[0].changeset: 1092352e3a895f0ae9301a5e0637b74ae36763b57d3e907897e0e0408a07830b
…
2
{"ok":true,"op":"derive","api":1,"result":{"derived":15,"cached":0,"next":20},"meta":{"repo":"…","elapsed_ms":1,…}}
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":45,"changesets_checked":15,"problems":[],"findings":[],"truncated":false,"found":0},"meta":{"repo":"…","elapsed_ms":2,…}}
```

## What it saves

At the size of the pilot's store, SwarmOps' own 470 commits with a change landed on top, removing
`meta/derived/` took it from 92 MB in 17,230 files to 57 MB in 8,859 files (measured on 2026-10-20).
An export into an empty git directory stored its 8,371 values again and wrote the same head; an
export to the branch that already held everything wrote nothing and still stored 2,544 of them. So
the values trade room for the time a later export spends deriving them, and the objects, most of a
store, stay.
