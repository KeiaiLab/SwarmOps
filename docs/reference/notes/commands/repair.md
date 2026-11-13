`repair` mends what the journal can rebuild and sets aside what it cannot. It takes the store's
maintenance lease, so a `gc`, `repair` or `upgrade` already running makes it fail at once with
the retryable `E_MAINTENANCE_BUSY` (exit 4). It never cuts the journal: damage in the journal
past a torn tail is refused as `E_STORE_CORRUPT`, with the frame's offset in `details.offset`.
Run it with `--dry-run` first: it lists the actions, each with `done: false`, and writes nothing.

### The example

The store of the example above is a FORMAT 1 store with three commits, whose `meta/journal.off`
had the entry of its middle frame zeroed. That is not a torn tail, which every command mends
when it opens the store, so `verify` fails with `E_STORE_CORRUPT` until `repair` writes
`journal.off` again from `journal.log`. A FORMAT 2 or 3 store keeps its frames' entries in
`meta/journal2.off`.

A missing `journal.off` is not mended: every command fails to open the store with `E_IO`
(`details.store_file` `meta/journal.off`), `repair` too. Restore the store from a backup.

### Actions

Each action names what it worked on by an object id, a ref name or a kind of file, never a
host path:

| `kind` | `target` | What it does |
| --- | --- | --- |
| `rebuild_offsets` | `journal_offsets` | Writes `journal.off` again from the journal. |
| `rebuild_index` | `changeset_index` | Replays the journal into the changeset index and its sorted views. |
| `rebuild_ref` | the ref's name | Writes the ref's file again from the journal. |
| `quarantine_object` | the object's id | Moves an object file that does not hash to its id, or does not decode, to `quarantine/objects/`. |
| `drop_derived` | `<name>:<subject>` | Removes a derived value that fails its checks: a cache. |
| `drop_external_id` | `<system>:<key>` | Removes an external id entry that is damaged or names no changeset of the store. |

`after` is a deep `verify` once the actions are done. A quarantined object it still counts in
`found` comes back when its content is stored again (`blob put`, a commit or a `git import`);
where nothing holds the content, restore the store from a backup.
