`id` is an operation's id from `op_log` (`4:9f3a01c2`): its number and its frame's crc.
A bare number, an upper-case crc or any other form is `E_INVALID_ARG`, and an id whose crc is
not the frame's at that number is `E_OP_NOT_FOUND` (`reason: checksum`).
It moves every ref that operation moved back to what it named before, by compare-and-swap in
one transaction: all of them or none.
A ref moved since stops it (`E_CAS_MISMATCH`, nothing written): undo the later operations first.
An undo that would move `main` off its first-parent history is `E_REF_PROTECTED` (nothing
written): land a change that reverts it instead.
An undo run again is a replay (`already_applied`).
Changesets are never removed.
