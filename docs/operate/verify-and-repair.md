# Verify and repair

Check a store for damage, read what a failed check names, and repair a damaged store.
Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

## Before you start

- A store ([Stores](stores.md)); `swarmops verify` needs no identity.

## Check a store

```sh
swarmops verify
swarmops verify --deep
```

```json
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":42,"changesets_checked":14,"problems":[],"findings":[],"truncated":false,"found":0},"meta":{"repo":"…","elapsed_ms":2,"resolved":{},"auth":"trust"}}
```

`verify` checks the structure: the journal, the index, the refs, every changeset and tree, the
external id entries, and that every blob is there. `--deep` also re-hashes every object file. On
SwarmOps' own history (7,647 objects) they took 0.1 s and 0.3 s in the pilot's dry run of
2026-10-20, and 0.09 s and 0.23 s for 8,368 objects, measured again later that day at a load
average of about 20.

A report lists at most 1,000 problems (`--max-findings` asks for fewer): `found` counts every
problem found, and `truncated` says that some are not listed.

## Read a failed check

A store with damage fails with
`E_STORE_CORRUPT` (exit 1), each damaged object, record or ref in `details.findings`; this is
`verify --deep --pretty` on a copy of the store with one byte of one object file changed:

```text
{
  "ok": false,
  "op": "verify",
  "api": 1,
  "error": {
    "code": "E_STORE_CORRUPT",
    "message": "verify found a problem: object 0112121a9368ae739fcf8af5e4a1dd4f94d76a6afa38135b1ba08cc5cdadbbfa is corrupt",
    "retryable": false,
    "exit": 1,
    "details": {
      "changesets_checked": 14,
      "findings": [
        {
          "kind": "corrupt_object",
          "object": "0112121a9368ae739fcf8af5e4a1dd4f94d76a6afa38135b1ba08cc5cdadbbfa"
        }
      ],
      "found": 1,
      "objects_checked": 42,
      "problems": [
        "object 0112121a9368ae739fcf8af5e4a1dd4f94d76a6afa38135b1ba08cc5cdadbbfa is corrupt"
      ],
      "truncated": false
    },
    "hint": "details.findings names each damaged object, record or ref by kind and id: restore them from a backup of the store"
  },
  …
}
```

## Repair a damaged store

`swarmops repair` mends what the journal can rebuild and sets aside what it cannot. It takes the
store's maintenance lease (another `gc`, `repair` or `upgrade` fails at once with the retryable
`E_MAINTENANCE_BUSY`), then, in this order:

1. writes `journal.off` again from `journal.log` if it is missing or out of step;
2. runs `verify --deep`;
3. rebuilds the ref files, then the changeset index and its sorted views, from the journal;
4. moves each damaged object file to `quarantine/objects/<id>`;
5. drops damaged derived values and external id entries, which are caches;
6. runs `verify --deep` again, and reports it as `after`.

It never cuts the journal: damage in `journal.log` past a torn tail is refused as
`E_STORE_CORRUPT`, with the frame's offset in `details.offset`, and the store is restored from a
backup ([Restore a store](backup-and-restore.md#restore-a-store)). Run it with `--dry-run` first,
which lists the actions (`done: false`) and writes nothing. This is a store whose only blob file
had one byte changed:

```sh
swarmops repair --dry-run
swarmops repair
```

```text
{"ok":true,"op":"repair","api":1,"result":{"actions":[{"kind":"quarantine_object","target":"2112321ed0469c980077fcc513e5e27eb883aa6edaa409d15b974e5d4cace80a","done":false}],"after":{"objects_checked":3,"changesets_checked":1,"found":1}},…}
{"ok":true,"op":"repair","api":1,"result":{"actions":[{"kind":"quarantine_object","target":"2112321ed0469c980077fcc513e5e27eb883aa6edaa409d15b974e5d4cace80a","done":true}],"after":{"objects_checked":2,"changesets_checked":1,"found":1}},…}
```

`after` still finds one problem: the changeset names an object the store no longer holds. A
quarantined object comes back when its content is stored again: `blob put` of the file for a blob,
or a commit or `git import` that makes the same tree or changeset. Then `verify --deep` is clean:

```sh
swarmops blob put hi.txt
swarmops verify --deep
```

```text
{"ok":true,"op":"blob_put","api":1,"result":{"blob":"2112321ed0469c980077fcc513e5e27eb883aa6edaa409d15b974e5d4cace80a","size":3,"written":true},…}
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":3,"changesets_checked":1,"problems":[],"findings":[],"truncated":false,"found":0},…}
```

Where nothing holds the content, restore the store from a backup. `quarantine/` is kept: neither
`repair` nor `gc` deletes it.

## When to run it

Run `verify --deep` after a crash or a power loss, on every backup, before an upgrade,
and on a schedule: it reads the whole store, and holds the journal and the index in memory.
