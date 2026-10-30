Each entry is one write transaction of the store, newest first, with the `id` that `undo` takes,
its kind, the changesets it recorded and the refs it moved.
Pass a `limit` of 20 or less and page on with `before`.
