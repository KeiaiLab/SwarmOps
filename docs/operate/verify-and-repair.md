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
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":42,"changesets_checked":14,"problems":[],"findings":[]},"meta":{"repo":"…","elapsed_ms":2,"resolved":{},"auth":"trust"}}
```

`verify` checks the structure: the journal, the index, the refs, every changeset and tree, the
external id entries, and that every blob is there. `--deep` also re-hashes every object file. On
SwarmOps' own history (7,647 objects) they took 0.1 s and 0.3 s in the pilot's dry run of
2026-10-20, and 0.09 s and 0.23 s for 8,368 objects, measured again later that day at a load
average of about 20.

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
      "objects_checked": 42,
      "problems": [
        "object 0112121a9368ae739fcf8af5e4a1dd4f94d76a6afa38135b1ba08cc5cdadbbfa is corrupt"
      ]
    },
    "hint": "details.findings names each damaged object, record or ref by kind and id: restore them from a backup of the store"
  },
  …
}
```

## Repair a damaged store

There is no repair tool yet (M6): stop the writers, keep the damaged store aside, and restore
([Restore a store](backup-and-restore.md#restore-a-store)).

## When to run it

Run `verify --deep` after a crash or a power loss, on every backup, before an upgrade,
and on a schedule: it reads the whole store, and holds the journal and the index in memory.
