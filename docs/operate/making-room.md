# Making room

Remove, by hand and with every writer stopped, the two kinds of file a store may lose before
`swarmops gc` exists.
Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

## What may go

A store only grows ([Limits](limits.md)): nothing is collected until M6's `swarmops gc`. Two kinds of
file in it may go by hand meanwhile, with every writer stopped. This is what the storage spec gives
for each, and no more:

- **The leftovers of interrupted writes**, files in `objects/tmp/` and `meta/derived/tmp/`. A write
  stages its file there and links it into place, and a writer that dies on the way leaves the file.
  The spec has a crash before the frame that names an object leave only orphan objects and temp
  files, and no frame names an object before it is linked and durable (§2, rules 1 and 2), so a
  staging file is never the only name of an object that a changeset needs. `swarmops gc` is to clear
  the files of `objects/tmp/` that are older than an hour (§2) and the temp files of `meta/derived/`
  (§3). By hand, remove the files older than an hour, and not the directories: the spec lists them
  as part of the layout, and does not say that a store makes them again.
- **The derived values**, `meta/derived/`, which may go whole. They are a cache that neither the
  journal nor any other file names (§3), and removing a value's file, or `meta/derived/`, makes the
  value derived again (§5).

The orphan objects stay: telling them from the objects a changeset names takes the mark and sweep
from the journal that `gc` gets later. The objects, the journal, the index and the refs stay as they
are.

## 1. Stop every writer

Both removals need the store to themselves. A writer is every process that can write it: the agents'
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

From the store's directory (the `:?` refuses an empty variable, which `cd` would take for the
current directory), the leftovers (on a store that never derived a value, `find` says that `meta/derived/tmp` does not exist, which is right):

```sh
cd "${SWARMOPS_REPO:?}"
find objects/tmp meta/derived/tmp -type f -mmin +60
```

```text
objects/tmp/.tmpUbSpKg
meta/derived/tmp/3849408-0-ffc434ac1bd4cb92
```

This store had three leftovers of writers that were killed in the middle of a write; two were set
back a day with `touch -d`, to have the hour pass, and the third is as young as it was. The removal
leaves that one:

```sh
find objects/tmp meta/derived/tmp -type f -mmin +60 -delete
find objects/tmp meta/derived/tmp -type f
```

```text
objects/tmp/.tmpoyk2fg
```

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
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":45,"changesets_checked":15,"problems":[],"findings":[]},"meta":{"repo":"…","elapsed_ms":2,…}}
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
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":45,"changesets_checked":15,"problems":[],"findings":[]},"meta":{"repo":"…","elapsed_ms":2,…}}
```

## What it saves

At the size of the pilot's store, SwarmOps' own 470 commits with a change landed on top, removing
`meta/derived/` took it from 92 MB in 17,230 files to 57 MB in 8,859 files (measured on 2026-10-20).
An export into an empty git directory stored its 8,371 values again and wrote the same head; an
export to the branch that already held everything wrote nothing and still stored 2,544 of them. So
the values trade room for the time a later export spends deriving them, and the objects, most of a
store, stay.
