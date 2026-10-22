Each entry is one write transaction of the store, newest first, numbered as `undo` takes it, with
its kind, the changesets it recorded and the refs it moved.
Pass a `limit` of 20 or less and page on with `before`.
