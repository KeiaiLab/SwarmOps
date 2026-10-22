Without `deep`, it checks structure: parents, that referenced objects exist, the index against
the journal, and refs.
With `deep: true` it also re-hashes every object and checks the derived values against the
objects.
A store with problems is `E_STORE_CORRUPT`, as `swarmops verify` is.
