`op` is an operation's number from `op_log`.
It moves every ref that operation moved back to what it named before, by compare-and-swap in
one transaction: all of them or none.
A ref moved since stops it (`E_CAS_MISMATCH`, nothing written): undo the later operations first.
An undo run again is a replay (`already_applied`).
Changesets are never removed.
